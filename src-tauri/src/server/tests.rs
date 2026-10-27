//! End-to-end tests: the real router in front of mock OpenAI/Anthropic upstreams.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::{router, Ctx, Server, Timing};
use crate::config::AppConfig;
use crate::convert::sse::SseParser;
use crate::convert::stream::{tests::accumulate, Event};
use crate::mapping::{ModelRule, Tier};
use crate::providers::Provider;
use crate::state::{test_core, Core};

#[derive(Default)]
struct Seen {
    body: Option<Value>,
    headers: Option<HeaderMap>,
}

type Shared = Arc<Mutex<Seen>>;

async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A streamed body: (delay before chunk in ms, chunk).
fn timed_body(chunks: Vec<(u64, String)>) -> Body {
    let s = futures_util::stream::iter(chunks).then(|(delay, chunk)| async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Ok::<Bytes, Infallible>(Bytes::from(chunk))
    });
    Body::from_stream(s)
}

fn sse(status: StatusCode, chunks: Vec<(u64, String)>) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(timed_body(chunks))
        .unwrap()
}

fn data(v: Value) -> String {
    format!("data: {v}\n\n")
}

fn record(seen: &Shared, headers: &HeaderMap, body: &Value) {
    let mut s = seen.lock().unwrap();
    s.body = Some(body.clone());
    s.headers = Some(headers.clone());
}

fn config(base: &str) -> AppConfig {
    AppConfig {
        providers: vec![Provider {
            id: "p1".into(),
            name: "Mock".into(),
            preset: "custom".into(),
            base_url: base.into(),
            ..Default::default()
        }],
        rules: vec![ModelRule {
            id: "r1".into(),
            local_id: "claude-sonnet-5.5".into(),
            provider_id: "p1".into(),
            upstream_model: "deepseek-v4.1-flash".into(),
            enabled: true,
            tier: Some(Tier::Sonnet),
            created_at: "2026-10-18T00:00:00Z".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

struct Gateway {
    url: String,
    core: Arc<Core>,
    http: reqwest::Client,
}

impl Gateway {
    async fn start(cfg: AppConfig, timing: Timing) -> Self {
        let core = test_core(cfg);
        core.set_provider_key("p1", Some("upstream-secret")).unwrap();
        let url = spawn(router(Ctx { core: core.clone(), cancel: CancellationToken::new(), timing })).await;
        Self { url, core, http: reqwest::Client::new() }
    }

    fn post(&self, path: &str, body: Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.url))
            .header("authorization", format!("Bearer {}", self.core.local_key()))
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20,interleaved-thinking-2025-05-14,some-future-beta")
            .json(&body)
    }
}

fn fast() -> Timing {
    Timing { ping: Duration::from_millis(150), header_wait: Duration::from_millis(100) }
}

fn request(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-5.5",
        "max_tokens": 1024,
        "stream": stream,
        "system": [{"type": "text", "text": "You are terse.", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Hi", "cache_control": {"type": "ephemeral"}}]}],
        "tools": [{"name": "get_weather", "description": "w", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
        "unknown_future_field": true
    })
}

/// Splits our SSE output into events; pings are counted and left out.
async fn read_events(resp: reqwest::Response) -> (Vec<Event>, usize) {
    let bytes = resp.bytes().await.unwrap();
    let mut parser = SseParser::new();
    let mut events = Vec::new();
    let mut pings = 0;
    for ev in parser.feed(&bytes).into_iter().chain(parser.finish()) {
        let name = ev.event.clone().expect("every event is named");
        let data: Value = serde_json::from_str(&ev.data).unwrap();
        if name == "ping" {
            pings += 1;
            continue;
        }
        events.push(Event { name: Box::leak(name.into_boxed_str()), data });
    }
    (events, pings)
}

#[tokio::test]
async fn auth_bearer_or_x_api_key() {
    let gw = Gateway::start(config("http://127.0.0.1:9"), fast()).await;
    let url = format!("{}/v1/models", gw.url);
    let none = gw.http.get(&url).send().await.unwrap();
    assert_eq!(none.status(), 401);
    let body: Value = none.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");

    let wrong = gw.http.get(&url).header("x-api-key", "nope").send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    let x = gw.http.get(&url).header("x-api-key", gw.core.local_key()).send().await.unwrap();
    assert_eq!(x.status(), 200);
    let bearer = gw.http.get(&url).bearer_auth(gw.core.local_key()).send().await.unwrap();
    assert_eq!(bearer.status(), 200);
    let models: Value = bearer.json().await.unwrap();
    assert_eq!(models["data"][0]["id"], "claude-sonnet-5.5");
    assert_eq!(models["data"][0]["anthropic_family_tier"], "sonnet");
    assert_eq!(models["has_more"], false);

    let one = gw
        .http
        .get(format!("{}/v1/models/claude-sonnet-5.5", gw.url))
        .bearer_auth(gw.core.local_key())
        .send()
        .await
        .unwrap();
    assert_eq!(one.json::<Value>().await.unwrap()["type"], "model");
}

#[tokio::test]
async fn unknown_model_and_unknown_route() {
    let gw = Gateway::start(config("http://127.0.0.1:9"), fast()).await;
    let mut req = request(false);
    req["model"] = json!("claude-opus-9");
    let resp = gw.post("/v1/messages", req).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("claude-opus-9"));

    let resp = gw.post("/v1/files", json!({})).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.json::<Value>().await.unwrap()["type"], "error");

    let resp = gw.post("/v1/messages/count_tokens", request(false)).send().await.unwrap();
    assert!(resp.json::<Value>().await.unwrap()["input_tokens"].as_u64().unwrap() > 0);

    // Claude Code's warm-up probe gets a 200 and stays out of the logs.
    let resp = gw.http.head(format!("{}/api/hello", gw.url)).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let logs = gw.core.logs.list();
    assert!(logs.iter().any(|l| l.status == 404 && l.local_model == "claude-opus-9"));
    assert!(!logs.iter().any(|l| l.path == "/api/hello"));
}

#[tokio::test]
async fn openai_non_stream_is_converted_both_ways() {
    let seen: Shared = Default::default();
    let mock = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(seen): State<Shared>, headers: HeaderMap, Json(body): Json<Value>| async move {
                record(&seen, &headers, &body);
                Json(json!({
                    "id": "cmpl-1",
                    "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                        "role": "assistant", "content": "Let me check.",
                        "tool_calls": [{"id": "call_9", "type": "function",
                            "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]}}],
                    "usage": {"prompt_tokens": 40, "completion_tokens": 9}
                }))
            }),
        )
        .with_state(seen.clone());
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;

    let resp = gw.post("/v1/messages", request(false)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let msg: Value = resp.json().await.unwrap();
    assert_eq!(msg["model"], "claude-sonnet-5.5");
    assert_eq!(msg["stop_reason"], "tool_use");
    assert_eq!(msg["content"][1]["input"], json!({"city": "Oslo"}));
    assert_eq!(msg["usage"]["input_tokens"], 40);
    assert_eq!(msg["usage"]["cache_read_input_tokens"], 0);

    let seen = seen.lock().unwrap();
    let body = seen.body.as_ref().unwrap();
    assert_eq!(body["model"], "deepseek-v4.1-flash");
    assert_eq!(body["messages"][0]["content"], "You are terse.");
    assert!(!body.to_string().contains("cache_control"));
    assert!(body.get("unknown_future_field").is_none());
    assert_eq!(seen.headers.as_ref().unwrap()["authorization"], "Bearer upstream-secret");

    let log = &gw.core.logs.list()[0];
    assert_eq!((log.status, log.input_tokens, log.output_tokens), (200, 40, 9));
    assert_eq!(log.upstream_model, "deepseek-v4.1-flash");
    assert_eq!(log.provider, "Mock");
}

#[tokio::test]
async fn aggregator_rule_pins_the_sub_provider() {
    let seen: Shared = Default::default();
    let mock = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(seen): State<Shared>, headers: HeaderMap, Json(body): Json<Value>| async move {
                record(&seen, &headers, &body);
                Json(json!({
                    "id": "cmpl-1",
                    "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "ok"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                }))
            }),
        )
        .with_state(seen.clone());
    let base = spawn(mock).await;
    let mut cfg = config(&format!("{base}/v1"));
    cfg.rules[0].sub_provider = "DeepInfra".into();

    // A plain provider ignores the sub-provider.
    let gw = Gateway::start(cfg.clone(), fast()).await;
    assert_eq!(gw.post("/v1/messages", request(false)).send().await.unwrap().status(), 200);
    assert!(seen.lock().unwrap().body.as_ref().unwrap().get("provider").is_none());

    cfg.providers[0].aggregator = true;
    let gw = Gateway::start(cfg, fast()).await;
    assert_eq!(gw.post("/v1/messages", request(false)).send().await.unwrap().status(), 200);
    assert_eq!(seen.lock().unwrap().body.as_ref().unwrap()["provider"], json!({"only": ["DeepInfra"]}));
}

fn openai_request(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-5.5",
        "stream": stream,
        "max_completion_tokens": 500,
        "messages": [{"role": "user", "content": "Hi"}]
    })
}

#[tokio::test]
async fn openai_clients_get_chat_completions_passed_through() {
    let seen: Shared = Default::default();
    let mock = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(seen): State<Shared>, headers: HeaderMap, Json(body): Json<Value>| async move {
                record(&seen, &headers, &body);
                Json(json!({
                    "id": "cmpl-1", "object": "chat.completion", "model": "deepseek-v4.1-flash",
                    "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "Hello!"}}],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 3}
                }))
            }),
        )
        .with_state(seen.clone());
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;

    // Both spellings work: clients differ on whether the base URL carries /v1.
    for path in ["/v1/chat/completions", "/chat/completions"] {
        let resp = gw.post(path, openai_request(false)).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(body["usage"]["prompt_tokens"], 12);
    }
    let seen = seen.lock().unwrap();
    let sent = seen.body.as_ref().unwrap();
    assert_eq!(sent["model"], "deepseek-v4.1-flash");
    assert_eq!(sent["max_tokens"], 500);
    assert!(sent.get("max_completion_tokens").is_none());
    assert_eq!(seen.headers.as_ref().unwrap()["authorization"], "Bearer upstream-secret");

    let log = &gw.core.logs.list()[0];
    assert_eq!((log.path.as_str(), log.status, log.input_tokens, log.output_tokens), ("/chat/completions", 200, 12, 3));
    assert_eq!((log.local_model.as_str(), log.upstream_model.as_str(), log.provider.as_str()), ("claude-sonnet-5.5", "deepseek-v4.1-flash", "Mock"));
}

#[tokio::test]
async fn openai_stream_is_forwarded_byte_for_byte() {
    let seen: Shared = Default::default();
    let mock = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(seen): State<Shared>, headers: HeaderMap, Json(body): Json<Value>| async move {
                record(&seen, &headers, &body);
                let delta = |text: &str| data(json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}));
                sse(
                    StatusCode::OK,
                    vec![
                        (0, delta("Hel")),
                        (0, delta("lo")),
                        (0, data(json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 4}}))),
                        (0, "data: [DONE]\n\n".to_string()),
                    ],
                )
            }),
        )
        .with_state(seen.clone());
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;

    let resp = gw.post("/v1/chat/completions", openai_request(true)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let text = resp.text().await.unwrap();
    assert!(text.contains(r#""content":"Hel""#) && text.contains(r#""content":"lo""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));
    assert_eq!(seen.lock().unwrap().body.as_ref().unwrap()["stream_options"]["include_usage"], true);

    // the log entry is written once the stream has ended
    let mut log = None;
    for _ in 0..50 {
        log = gw.core.logs.list().into_iter().next();
        if log.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = log.expect("stream was logged");
    assert_eq!((log.status, log.stream, log.input_tokens, log.output_tokens), (200, true, 20, 4));
}

#[tokio::test]
async fn openai_errors_come_back_in_openai_shape() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"message": "slow down", "type": "rate_limit_error", "code": "rate_limit"}})))
        }),
    );
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;

    let resp = gw.post("/v1/chat/completions", openai_request(false)).send().await.unwrap();
    assert_eq!(resp.status(), 429);
    let body: Value = resp.json().await.unwrap();
    assert_eq!((body["error"]["message"].as_str(), body["error"]["code"].as_str()), (Some("slow down"), Some("rate_limit")));

    let mut unknown = openai_request(false);
    unknown["model"] = json!("claude-opus-9");
    let resp = gw.post("/v1/chat/completions", unknown).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("claude-opus-9"));
    assert!(body.get("type").is_none(), "no Anthropic envelope on the OpenAI surface");

    let resp = gw.http.post(format!("{}/v1/chat/completions", gw.url)).json(&openai_request(false)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert!(resp.json::<Value>().await.unwrap()["error"]["message"].is_string());
}

#[tokio::test]
async fn models_list_works_for_openai_clients_too() {
    let gw = Gateway::start(config("http://127.0.0.1:9"), fast()).await;
    for path in ["/v1/models", "/models"] {
        let resp = gw.http.get(format!("{}{path}", gw.url)).bearer_auth(gw.core.local_key()).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["object"], "list");
        assert_eq!((body["data"][0]["id"].as_str(), body["data"][0]["object"].as_str()), (Some("claude-sonnet-5.5"), Some("model")));
    }
}

#[tokio::test]
async fn openai_stream_with_reasoning_and_tool_calls() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body["stream"], true);
            assert_eq!(body["stream_options"]["include_usage"], true);
            let d = |delta: Value| data(json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}));
            sse(
                StatusCode::OK,
                vec![
                    (0, d(json!({"role": "assistant", "reasoning_content": "Think"}))),
                    (0, d(json!({"content": "Sure. "}))),
                    (0, d(json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"ci"}}]}))),
                    // split mid-line to exercise the parser
                    (0, "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,".into()),
                    (0, "\"function\":{\"arguments\":\"ty\\\":\\\"Oslo\\\"}\"}}]},\"finish_reason\":null}]}\n\n".into()),
                    (0, data(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}))),
                    (0, data(json!({"choices": [], "usage": {"prompt_tokens": 50, "completion_tokens": 12,
                        "prompt_tokens_details": {"cached_tokens": 30}}}))),
                    (0, "data: [DONE]\n\n".into()),
                ],
            )
        }),
    );
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;
    let resp = gw.post("/v1/messages", request(true)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let (events, _) = read_events(resp).await;
    let msg = accumulate(&events);
    assert_eq!(msg["model"], "claude-sonnet-5.5");
    assert_eq!(
        msg["content"],
        json!([
            {"type": "thinking", "thinking": "Think", "signature": "rt:reasoning_content"},
            {"type": "text", "text": "Sure. "},
            {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Oslo"}}
        ])
    );
    assert_eq!(msg["stop_reason"], "tool_use");
    assert_eq!(
        msg["usage"],
        json!({"input_tokens": 20, "output_tokens": 12, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 30})
    );
    let log = &gw.core.logs.list()[0];
    assert!(log.stream);
    assert_eq!((log.status, log.output_tokens), (200, 12));
}

#[tokio::test]
async fn pings_while_upstream_is_silent() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            // Headers come late (past header_wait), then a long pause before the first token.
            tokio::time::sleep(Duration::from_millis(400)).await;
            sse(
                StatusCode::OK,
                vec![
                    (500, data(json!({"choices": [{"index": 0, "delta": {"content": "late"}, "finish_reason": "stop"}]}))),
                    (0, "data: [DONE]\n\n".into()),
                ],
            )
        }),
    );
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;
    let resp = gw.post("/v1/messages", request(true)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let (events, pings) = read_events(resp).await;
    assert!(pings >= 4, "expected pings during ~900ms of silence, got {pings}");
    assert_eq!(accumulate(&events)["content"][0]["text"], "late");
}

#[tokio::test]
async fn upstream_errors_keep_status_and_context_overflow_is_rewritten() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": "This model's maximum context length is 65536 tokens. However, your messages resulted in 70000 tokens.", "type": "invalid_request_error", "code": "context_length_exceeded"}})),
            )
        }),
    );
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;
    for stream in [false, true] {
        let resp = gw.post("/v1/messages", request(stream)).send().await.unwrap();
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"].as_str().unwrap().starts_with("prompt is too long: 70000 tokens > 65536 maximum"));
    }

    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"message": "slow down"}}))) }),
    );
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;
    let resp = gw.post("/v1/messages", request(true)).send().await.unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.json::<Value>().await.unwrap()["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn late_upstream_error_becomes_sse_error_event() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": {"message": "no providers"}})))
        }),
    );
    let base = spawn(mock).await;
    let gw = Gateway::start(config(&format!("{base}/v1")), fast()).await;
    let resp = gw.post("/v1/messages", request(true)).send().await.unwrap();
    assert_eq!(resp.status(), 200, "committed to SSE before the upstream answered");
    let (events, _) = read_events(resp).await;
    assert_eq!(events[0].name, "message_start");
    let last = events.last().unwrap();
    assert_eq!(last.name, "error");
    assert_eq!(last.data["error"]["type"], "overloaded_error");
    assert_eq!(last.data["error"]["message"], "no providers");
    assert_eq!(gw.core.logs.list()[0].status, 503);
}

#[tokio::test]
async fn unreachable_upstream_is_a_502() {
    let gw = Gateway::start(config("http://127.0.0.1:9/v1"), fast()).await;
    let resp = gw.post("/v1/messages", request(false)).send().await.unwrap();
    assert_eq!(resp.status(), 502);
    assert_eq!(resp.json::<Value>().await.unwrap()["error"]["type"], "api_error");
}

#[tokio::test]
async fn server_start_stop_and_busy_port() {
    let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let core = test_core(AppConfig { port, ..Default::default() });
    let server = Server::default();
    server.start(core.clone()).await.unwrap();
    assert!(server.status().running);
    let body: Value = reqwest::get(format!("http://127.0.0.1:{port}/")).await.unwrap().json().await.unwrap();
    assert_eq!(body["name"], "Retranslator");

    let other = Server::default();
    let err = other.start(core.clone()).await.unwrap_err();
    assert!(err.contains(&port.to_string()));
    assert!(other.status().error.is_some());

    server.stop(&core).await;
    assert!(!server.status().running);
    assert!(reqwest::get(format!("http://127.0.0.1:{port}/")).await.is_err());
    server.start(core.clone()).await.unwrap();
    server.stop(&core).await;
}
