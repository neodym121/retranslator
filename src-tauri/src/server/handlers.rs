//! HTTP handlers for the Anthropic-compatible surface.

use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::relay::{self, Call};
use super::Ctx;
use crate::convert::errors::ApiError;
use crate::convert::tokens::estimate_request_tokens;
use crate::logs::LogEntry;
use crate::mapping;

pub fn error_response(err: &ApiError) -> Response {
    let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(err.body())).into_response()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Accepts `x-api-key: <key>` or `Authorization: Bearer <key>`.
pub(super) fn authorize(ctx: &Ctx, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = ctx.core.local_key();
    let x_api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(str::trim);
    let bearer = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| {
        let v = v.trim();
        let (scheme, token) = v.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    });
    if x_api_key.is_none() && bearer.is_none() {
        return Err(ApiError::new(
            401,
            "authentication_error",
            "missing API key: send Authorization: Bearer <key> or x-api-key: <key>",
        ));
    }
    if [x_api_key, bearer].into_iter().flatten().any(|k| constant_time_eq(k, &expected)) {
        Ok(())
    } else {
        Err(ApiError::new(401, "authentication_error", "invalid x-api-key: the Retranslator key does not match"))
    }
}

fn record_simple(ctx: &Ctx, method: &str, path: &str, started: Instant, status: u16, error: Option<String>) {
    ctx.core.record(LogEntry {
        method: method.into(),
        path: path.into(),
        status,
        duration_ms: started.elapsed().as_millis() as u64,
        error,
        ..Default::default()
    });
}

pub async fn root() -> Json<Value> {
    Json(json!({ "name": "Retranslator", "version": env!("CARGO_PKG_VERSION"), "status": "ok" }))
}

/// `HEAD /api/hello`: Claude Code's connection-warming probe. Answered, not logged.
pub async fn hello() -> StatusCode {
    StatusCode::OK
}

pub async fn messages(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    let mut call = Call::new(ctx.clone(), "/v1/messages");
    if let Err(err) = authorize(&ctx, &headers) {
        return call.fail(err);
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return call.fail(ApiError::new(400, "invalid_request_error", "request body must be a JSON object")),
        Err(e) => return call.fail(ApiError::new(400, "invalid_request_error", format!("invalid JSON: {e}"))),
    };
    let model = value.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    call.entry.stream = value.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if model.is_empty() {
        return call.fail(ApiError::new(400, "invalid_request_error", "model: Field required"));
    }
    call.entry.local_model = model.clone();
    let route = match mapping::resolve(&ctx.core.config(), &model) {
        Ok(route) => route,
        Err(e) => return call.fail(ApiError::new(404, "not_found_error", e.message())),
    };
    call.entry.upstream_model = route.rule.upstream_model.clone();
    call.entry.provider = route.provider.name.clone();
    let key = ctx.core.provider_key(&route.provider.id);
    relay::relay(call, route, key, value).await
}

pub async fn count_tokens(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(err) = authorize(&ctx, &headers) {
        return error_response(&err);
    }
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.is_object() => Json(json!({ "input_tokens": estimate_request_tokens(&v) })).into_response(),
        _ => error_response(&ApiError::new(400, "invalid_request_error", "request body must be a JSON object")),
    }
}

pub async fn list_models(State(ctx): State<Ctx>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let started = Instant::now();
    if let Err(err) = authorize(&ctx, &headers) {
        record_simple(&ctx, "GET", uri.path(), started, err.status, Some(err.message.clone()));
        return error_response(&err);
    }
    let body = mapping::models_response(&ctx.core.config());
    record_simple(&ctx, "GET", uri.path(), started, 200, None);
    Json(body).into_response()
}

pub async fn get_model(State(ctx): State<Ctx>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(err) = authorize(&ctx, &headers) {
        return error_response(&err);
    }
    let wanted = mapping::strip_1m_suffix(&id).to_string();
    let list = mapping::models_response(&ctx.core.config());
    let found = list["data"]
        .as_array()
        .and_then(|d| d.iter().find(|m| m["id"].as_str().is_some_and(|i| i.eq_ignore_ascii_case(&wanted))).cloned());
    match found {
        Some(model) => Json(model).into_response(),
        None => error_response(&ApiError::new(404, "not_found_error", format!("model: {id}"))),
    }
}

pub async fn not_found(State(ctx): State<Ctx>, method: Method, uri: Uri) -> Response {
    let err = ApiError::new(404, "not_found_error", format!("Retranslator: no route for {method} {}", uri.path()));
    record_simple(&ctx, method.as_str(), uri.path(), Instant::now(), 404, None);
    error_response(&err)
}
