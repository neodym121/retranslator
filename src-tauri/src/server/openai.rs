//! OpenAI-compatible surface: `POST /v1/chat/completions`. The request already speaks the provider's
//! language, so it is forwarded as it is: only the model id, the output cap and the routing hint change,
//! and the answer (stream or not) goes back untouched.

use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::handlers::authorize;
use super::relay::{unreachable_error, Call};
use super::Ctx;
use crate::convert::errors::{describe_reqwest, normalize_stream_error, normalize_upstream_error, ApiError};
use crate::convert::response::{map_usage, Usage};
use crate::convert::sse::SseParser;
use crate::i18n::t;
use crate::mapping::{self, Route};
use crate::providers::{endpoint_url, quirks, upstream_headers, Endpoint};

fn json_response(status: u16, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
        .header(CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

/// OpenAI error body: `{"error": {"message", "type", "param", "code"}}`.
fn error_response(status: u16, kind: &str, message: &str) -> Response {
    let body = json!({ "error": { "message": message, "type": kind, "param": null, "code": null } });
    json_response(status, Bytes::from(body.to_string()))
}

fn fail(call: Call, err: ApiError) -> Response {
    let resp = error_response(err.status, &err.kind, &err.message);
    call.done(err.status, Usage::default(), Some(err.message));
    resp
}

/// An upstream error keeps its status; a JSON error body from an OpenAI-style provider goes through as is.
fn upstream_failure(call: Call, status: u16, bytes: Bytes) -> Response {
    let err = normalize_upstream_error(status, &bytes);
    let own_shape = serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v.get("error").is_some());
    let resp = if own_shape { json_response(status, bytes) } else { error_response(err.status, &err.kind, &err.message) };
    call.done(err.status, Usage::default(), Some(err.message));
    resp
}

/// The request as the provider gets it: real model id, capped output, sub-provider pin.
pub(super) fn upstream_body(mut body: Map<String, Value>, route: &Route) -> Value {
    let quirks = quirks(&route.provider.base_url);
    body.insert("model".into(), json!(route.rule.upstream_model));

    // Clients name the cap either way; the provider gets the name it understands.
    let asked = ["max_tokens", "max_completion_tokens"]
        .iter()
        .filter_map(|k| body.remove(*k))
        .filter_map(|v| v.as_u64())
        .min();
    let cap = match (asked, route.rule.max_output_tokens) {
        (Some(asked), Some(cap)) => Some(asked.min(cap)),
        (asked, cap) => asked.or(cap),
    };
    if let Some(n) = cap {
        let field = if quirks.max_completion_tokens { "max_completion_tokens" } else { "max_tokens" };
        body.insert(field.into(), json!(n));
    }

    if let Some(sub) = route.sub_provider() {
        body.entry("provider").or_insert_with(|| json!({ "only": [sub] }));
    }

    // Token counts for the log come from the final chunk.
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let options = body.entry("stream_options").or_insert_with(|| json!({}));
        if let Some(options) = options.as_object_mut() {
            options.insert("include_usage".into(), json!(true));
        }
    }
    Value::Object(body)
}

pub async fn chat_completions(State(ctx): State<Ctx>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> Response {
    let mut call = Call::new(ctx.clone(), uri.path());
    if let Err(err) = authorize(&ctx, &headers) {
        return fail(call, err);
    }
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&body) else {
        return fail(call, ApiError::new(400, "invalid_request_error", "request body must be a JSON object"));
    };
    let model = map.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if model.is_empty() {
        return fail(call, ApiError::new(400, "invalid_request_error", "you must provide a model parameter"));
    }
    let stream = map.get("stream").and_then(Value::as_bool).unwrap_or(false);
    call.entry.stream = stream;
    call.entry.local_model = model.clone();
    let route = match mapping::resolve(&ctx.core.config(), &model) {
        Ok(route) => route,
        Err(e) => return fail(call, ApiError::new(404, "not_found_error", e.message())),
    };
    call.entry.upstream_model = route.rule.upstream_model.clone();
    call.entry.provider = route.provider.name.clone();

    let key = ctx.core.provider_key(&route.provider.id);
    let url = endpoint_url(&route.provider.base_url, Endpoint::ChatCompletions);
    let upstream = upstream_body(map, &route);
    let resp = match ctx.core.http.post(url).headers(upstream_headers(&route.provider, &key)).json(&upstream).send().await {
        Ok(r) => r,
        Err(e) => return fail(call, unreachable_error(&e)),
    };
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let bytes = resp.bytes().await.unwrap_or_default();
        return upstream_failure(call, status, bytes);
    }
    if stream {
        return stream_response(call, resp, ctx.cancel.clone());
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return fail(call, unreachable_error(&e)),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        // Some providers answer 200 with an error object.
        Ok(json) if json.get("error").is_some() && json.get("choices").is_none() => fail(call, normalize_stream_error(&json)),
        Ok(json) => {
            let usage = json.get("usage").filter(|u| u.is_object()).map(map_usage).unwrap_or_default();
            call.done(200, usage, None);
            json_response(200, bytes)
        }
        Err(_) => fail(call, ApiError::new(502, "api_error", "upstream returned a non-JSON response")),
    }
}

fn stream_response(call: Call, resp: reqwest::Response, cancel: CancellationToken) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(pump(call, resp, tx, cancel));
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

/// Copies the provider's stream to the client byte for byte, reading the usage out of it on the way.
async fn pump(call: Call, resp: reqwest::Response, tx: mpsc::Sender<Bytes>, cancel: CancellationToken) {
    let mut body = resp.bytes_stream();
    let mut parser = SseParser::new();
    let mut usage = Usage::default();
    let mut status = 200;
    let mut error = None;
    loop {
        tokio::select! {
            next = body.next() => match next {
                Some(Ok(chunk)) => {
                    for ev in parser.feed(&chunk) {
                        watch_usage(&ev.data, &mut usage);
                    }
                    if tx.send(chunk).await.is_err() {
                        (status, error) = (499, Some(gone()));
                        break;
                    }
                }
                Some(Err(e)) => {
                    let message = format!("upstream stream interrupted: {}", describe_reqwest(&e));
                    let event = json!({ "error": { "message": message, "type": "api_error", "param": null, "code": null } });
                    let _ = tx.send(Bytes::from(format!("data: {event}\n\n"))).await;
                    (status, error) = (502, Some(message));
                    break;
                }
                None => {
                    for ev in parser.finish() {
                        watch_usage(&ev.data, &mut usage);
                    }
                    break;
                }
            },
            _ = tx.closed() => {
                (status, error) = (499, Some(gone()));
                break;
            }
            _ = cancel.cancelled() => {
                (status, error) = (503, Some("Retranslator stopped".to_string()));
                break;
            }
        }
    }
    drop(body); // closes the upstream connection right away if the client left
    call.done(status, usage, error);
}

fn watch_usage(data: &str, usage: &mut Usage) {
    if let Some(u) = serde_json::from_str::<Value>(data.trim()).ok().and_then(|v| v.get("usage").cloned()).filter(Value::is_object) {
        *usage = map_usage(&u);
    }
}

fn gone() -> String {
    t("клиент закрыл соединение", "the client closed the connection").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::ModelRule;
    use crate::providers::Provider;

    fn route(base: &str, aggregator: bool, sub: &str, cap: Option<u64>) -> Route {
        Route {
            rule: ModelRule {
                upstream_model: "deepseek-v4.1-flash".into(),
                sub_provider: sub.into(),
                max_output_tokens: cap,
                ..Default::default()
            },
            provider: Provider { base_url: base.into(), aggregator, ..Default::default() },
        }
    }

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn model_is_swapped_and_the_rest_is_left_alone() {
        let out = upstream_body(
            body(json!({"model": "claude-sonnet-5.5", "messages": [{"role": "user", "content": "hi"}], "temperature": 0.3, "tools": []})),
            &route("https://x.test/v1", false, "", None),
        );
        assert_eq!(out["model"], "deepseek-v4.1-flash");
        assert_eq!(out["temperature"], 0.3);
        assert_eq!(out["messages"][0]["content"], "hi");
        assert!(out.get("max_tokens").is_none() && out.get("provider").is_none() && out.get("stream_options").is_none());
    }

    #[test]
    fn output_cap_uses_the_providers_field_name() {
        let r = route("https://x.test/v1", false, "", Some(8192));
        let out = upstream_body(body(json!({"model": "m", "max_completion_tokens": 64000})), &r);
        assert_eq!((out["max_tokens"].as_u64(), out.get("max_completion_tokens")), (Some(8192), None));
        // asked for less than the cap: the ask wins
        let out = upstream_body(body(json!({"model": "m", "max_tokens": 100})), &r);
        assert_eq!(out["max_tokens"], 100);
        // no ask: the cap applies
        assert_eq!(upstream_body(body(json!({"model": "m"})), &r)["max_tokens"], 8192);
        // api.openai.com wants max_completion_tokens
        let openai = route("https://api.openai.com/v1", false, "", None);
        let out = upstream_body(body(json!({"model": "m", "max_tokens": 50})), &openai);
        assert_eq!((out["max_completion_tokens"].as_u64(), out.get("max_tokens")), (Some(50), None));
    }

    #[test]
    fn sub_provider_pins_an_aggregator_but_never_overrides_the_client() {
        let r = route("https://openrouter.ai/api/v1", true, "DeepInfra", None);
        assert_eq!(upstream_body(body(json!({"model": "m"})), &r)["provider"], json!({"only": ["DeepInfra"]}));
        let own = json!({"order": ["Together"]});
        assert_eq!(upstream_body(body(json!({"model": "m", "provider": own})), &r)["provider"], own);
        let plain = route("https://x.test/v1", false, "DeepInfra", None);
        assert!(upstream_body(body(json!({"model": "m"})), &plain).get("provider").is_none());
    }

    #[test]
    fn streams_ask_for_usage() {
        let r = route("https://x.test/v1", false, "", None);
        let out = upstream_body(body(json!({"model": "m", "stream": true})), &r);
        assert_eq!(out["stream_options"]["include_usage"], true);
        let out = upstream_body(body(json!({"model": "m", "stream": true, "stream_options": {"x": 1}})), &r);
        assert_eq!((out["stream_options"]["x"].as_u64(), out["stream_options"]["include_usage"].as_bool()), (Some(1), Some(true)));
    }
}
