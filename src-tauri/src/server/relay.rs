//! Forwarding to the provider's OpenAI-compatible API, streaming or not.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use super::handlers::error_response;
use super::Ctx;
use crate::convert::anthropic::MessagesRequest;
use crate::convert::errors::{describe_reqwest, normalize_stream_error, normalize_upstream_error, ApiError};
use crate::convert::request::{request_tool_names, to_openai, OpenAiOptions};
use crate::convert::response::{openai_to_anthropic, Usage};
use crate::convert::sse::{event_bytes, ping_bytes, SseEvent, SseParser};
use crate::convert::stream::StreamConverter;
use crate::convert::tokens;
use crate::logs::LogEntry;
use crate::mapping::Route;
use crate::providers::{endpoint_url, quirks, upstream_headers, Endpoint};

/// One request's log entry, finished exactly once.
pub struct Call {
    ctx: Ctx,
    pub entry: LogEntry,
    started: Instant,
}

impl Call {
    pub fn new(ctx: Ctx, path: &str) -> Self {
        let entry = LogEntry { method: "POST".into(), path: path.into(), ..Default::default() };
        Self { ctx, entry, started: Instant::now() }
    }

    pub fn done(mut self, status: u16, usage: Usage, error: Option<String>) {
        self.entry.status = status;
        self.entry.duration_ms = self.started.elapsed().as_millis() as u64;
        self.entry.input_tokens = usage.input_tokens;
        self.entry.output_tokens = usage.output_tokens;
        self.entry.cache_read_tokens = usage.cache_read_input_tokens;
        self.entry.cache_write_tokens = usage.cache_creation_input_tokens;
        self.entry.error = error;
        self.ctx.core.record(self.entry);
    }

    pub fn fail(self, err: ApiError) -> Response {
        let resp = error_response(&err);
        self.done(err.status, Usage::default(), Some(err.message));
        resp
    }
}

pub(super) fn unreachable_error(e: &reqwest::Error) -> ApiError {
    let status = if e.is_timeout() { 504 } else { 502 };
    let kind = if e.is_timeout() { "timeout_error" } else { "api_error" };
    ApiError::new(status, kind, format!("upstream unreachable: {}", describe_reqwest(e)))
}

type SendFuture = Pin<Box<dyn Future<Output = reqwest::Result<reqwest::Response>> + Send>>;

pub async fn relay(call: Call, route: Route, key: String, value: Value) -> Response {
    let estimate = tokens::estimate_request_tokens(&value);
    let stream = call.entry.stream;
    let req: MessagesRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => return call.fail(ApiError::new(400, "invalid_request_error", format!("invalid request: {e}"))),
    };
    let opts = OpenAiOptions {
        model: route.rule.upstream_model.clone(),
        stream,
        max_tokens_cap: route.rule.max_output_tokens,
        quirks: quirks(&route.provider.base_url),
        sub_provider: route.sub_provider().map(str::to_string),
    };
    let body = to_openai(&req, &opts);
    let names = request_tool_names(&req);
    let url = endpoint_url(&route.provider.base_url, Endpoint::ChatCompletions);
    let builder = call.ctx.core.http.post(url).headers(upstream_headers(&route.provider, &key)).json(&body);

    if stream {
        let conv = StreamConverter::new(&call.entry.local_model, estimate).with_tool_names(names);
        return relay_stream(call, conv, Box::pin(builder.send())).await;
    }

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return call.fail(unreachable_error(&e)),
    };
    let status = resp.status().as_u16();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return call.fail(unreachable_error(&e)),
    };
    if !(200..300).contains(&status) {
        return call.fail(normalize_upstream_error(status, &bytes));
    }
    let json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return call.fail(ApiError::new(502, "api_error", "upstream returned a non-JSON response")),
    };
    // Some providers answer 200 with an error object.
    if json.get("error").is_some() && json.get("choices").is_none() {
        return call.fail(normalize_stream_error(&json));
    }
    let message = openai_to_anthropic(&json, &call.entry.local_model, &names);
    let usage = Usage::from_anthropic(&message["usage"]);
    call.done(200, usage, None);
    Json(message).into_response()
}

enum Upstream {
    Ready(reqwest::Response),
    Pending(SendFuture),
}

async fn relay_stream(call: Call, conv: StreamConverter, mut send: SendFuture) -> Response {
    let upstream = match tokio::time::timeout(call.ctx.timing.header_wait, &mut send).await {
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                let bytes = resp.bytes().await.unwrap_or_default();
                return call.fail(normalize_upstream_error(status, &bytes));
            }
            Upstream::Ready(resp)
        }
        Ok(Err(e)) => return call.fail(unreachable_error(&e)),
        Err(_) => Upstream::Pending(send),
    };
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(pump(call, conv, upstream, tx));
    let body = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok::<Bytes, Infallible>(chunk), rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(body))
        .unwrap()
}

/// Writes to the client and remembers when, for pings.
struct Out {
    tx: mpsc::Sender<Bytes>,
    last: Instant,
    ping: Duration,
}

impl Out {
    async fn send(&mut self, chunk: Bytes) -> bool {
        self.last = Instant::now();
        self.tx.send(chunk).await.is_ok()
    }

    fn ping_due(&self) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.last + self.ping)
    }
}

fn error_event(err: &ApiError) -> Bytes {
    event_bytes("error", &err.body())
}

enum Flow {
    Continue,
    End,
    Failed(ApiError),
    Gone,
}

async fn pump(call: Call, mut conv: StreamConverter, upstream: Upstream, tx: mpsc::Sender<Bytes>) {
    let cancel = call.ctx.cancel.clone();
    let mut out = Out { tx, last: Instant::now(), ping: call.ctx.timing.ping };
    let gone = || crate::i18n::t("клиент закрыл соединение", "the client closed the connection").to_string();

    if !out.send(conv.start().bytes()).await {
        return call.done(499, Usage::default(), Some(gone()));
    }

    let resp = match upstream {
        Upstream::Ready(r) => r,
        Upstream::Pending(mut send) => loop {
            tokio::select! {
                result = &mut send => {
                    let err = match result {
                        Ok(r) if r.status().is_success() => break r,
                        Ok(r) => {
                            let status = r.status().as_u16();
                            let bytes = r.bytes().await.unwrap_or_default();
                            normalize_upstream_error(status, &bytes)
                        }
                        Err(e) => unreachable_error(&e),
                    };
                    out.send(error_event(&err)).await;
                    return call.done(err.status, Usage::default(), Some(err.message));
                }
                _ = tokio::time::sleep_until(out.ping_due()) => {
                    if !out.send(ping_bytes()).await {
                        return call.done(499, Usage::default(), Some(gone()));
                    }
                }
                _ = out.tx.closed() => return call.done(499, Usage::default(), Some(gone())),
                _ = cancel.cancelled() => {
                    let err = ApiError::new(503, "overloaded_error", "Retranslator stopped");
                    out.send(error_event(&err)).await;
                    return call.done(err.status, Usage::default(), Some(err.message));
                }
            }
        },
    };

    let mut body = resp.bytes_stream();
    let mut parser = SseParser::new();
    let flow = 'read: loop {
        tokio::select! {
            next = body.next() => {
                let events = match next {
                    Some(Ok(chunk)) => parser.feed(&chunk),
                    Some(Err(e)) => break 'read Flow::Failed(ApiError::new(
                        502, "api_error", format!("upstream stream interrupted: {}", describe_reqwest(&e)))),
                    None => {
                        for ev in parser.finish() {
                            match handle_event(&mut conv, ev, &mut out).await {
                                Flow::Continue => {}
                                other => break 'read other,
                            }
                        }
                        break 'read Flow::End;
                    }
                };
                for ev in events {
                    match handle_event(&mut conv, ev, &mut out).await {
                        Flow::Continue => {}
                        other => break 'read other,
                    }
                }
            }
            _ = tokio::time::sleep_until(out.ping_due()) => {
                if !out.send(ping_bytes()).await {
                    break 'read Flow::Gone;
                }
            }
            _ = out.tx.closed() => break 'read Flow::Gone,
            _ = cancel.cancelled() => break 'read Flow::Failed(ApiError::new(503, "overloaded_error", "Retranslator stopped")),
        }
    };
    drop(body); // closes the upstream connection right away if the client left

    match flow {
        Flow::Gone => call.done(499, conv.usage(), Some(gone())),
        Flow::Failed(err) => {
            out.send(error_event(&err)).await;
            call.done(err.status, conv.usage(), Some(err.message));
        }
        Flow::End | Flow::Continue => {
            for ev in conv.finish() {
                if !out.send(ev.bytes()).await {
                    return call.done(499, conv.usage(), Some(gone()));
                }
            }
            call.done(200, conv.usage(), None);
        }
    }
}

async fn handle_event(conv: &mut StreamConverter, ev: SseEvent, out: &mut Out) -> Flow {
    let data = ev.data.trim();
    if data == "[DONE]" {
        return Flow::End;
    }
    let Ok(chunk) = serde_json::from_str::<Value>(data) else {
        return Flow::Continue; // keep-alive noise
    };
    if chunk.get("error").is_some() && chunk.get("choices").is_none() {
        return Flow::Failed(normalize_stream_error(&chunk));
    }
    for e in conv.push(&chunk) {
        if !out.send(e.bytes()).await {
            return Flow::Gone;
        }
    }
    Flow::Continue
}
