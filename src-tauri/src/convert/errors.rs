//! Upstream errors → Anthropic error format, including the context-overflow rewrite that
//! Claude Desktop needs to trigger automatic conversation compaction.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub kind: String,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, kind: &str, message: impl Into<String>) -> Self {
        Self { status, kind: kind.into(), message: message.into() }
    }

    pub fn body(&self) -> Value {
        json!({ "type": "error", "error": { "type": self.kind, "message": self.message } })
    }
}

/// Anthropic error type for an HTTP status.
pub fn kind_for_status(status: u16) -> &'static str {
    match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        408 | 504 => "timeout_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

const ANTHROPIC_KINDS: &[&str] = &[
    "invalid_request_error",
    "authentication_error",
    "billing_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
    "rate_limit_error",
    "timeout_error",
    "overloaded_error",
    "api_error",
];

/// Builds an Anthropic error from a non-2xx upstream response (any provider format).
pub fn normalize_upstream_error(status: u16, body: &[u8]) -> ApiError {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let (message, code, kind) = match &parsed {
        Some(v) => extract(v),
        None => (String::new(), None, None),
    };
    let message = if message.is_empty() {
        let text = String::from_utf8_lossy(body);
        let text = text.trim();
        if text.is_empty() {
            format!("upstream returned HTTP {status}")
        } else {
            truncate(text, 1000)
        }
    } else {
        message
    };
    finish(status, message, code.as_deref(), kind)
}

/// Builds an Anthropic error from an error object inside an SSE stream (status already 200).
pub fn normalize_stream_error(v: &Value) -> ApiError {
    let (message, code, kind) = extract(v);
    let status = v
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_u64)
        .filter(|c| (400..600).contains(c))
        .map(|c| c as u16)
        .unwrap_or(500);
    let message = if message.is_empty() { "upstream stream error".to_string() } else { message };
    finish(status, message, code.as_deref(), kind)
}

fn finish(status: u16, message: String, code: Option<&str>, kind: Option<String>) -> ApiError {
    if status != 429 && is_context_overflow(&message, code) {
        return ApiError::new(400, "invalid_request_error", prompt_too_long(&message));
    }
    let kind = kind
        .filter(|k| ANTHROPIC_KINDS.contains(&k.as_str()))
        .unwrap_or_else(|| kind_for_status(status).to_string());
    ApiError::new(status, &kind, message)
}

/// (message, code, anthropic error type) from Anthropic, OpenAI, Gemini or bare formats.
fn extract(v: &Value) -> (String, Option<String>, Option<String>) {
    let v = match v {
        Value::Array(items) => items.first().unwrap_or(v),
        other => other,
    };
    let err = v.get("error").unwrap_or(v);
    if let Some(s) = err.as_str() {
        return (s.to_string(), None, None);
    }
    let mut message = err.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
    // OpenRouter tucks the provider's real text into metadata.raw.
    if let Some(raw) = err.get("metadata").and_then(|m| m.get("raw")).and_then(Value::as_str) {
        if !raw.is_empty() && !message.contains(raw) {
            message = if message.is_empty() { raw.to_string() } else { format!("{message}: {raw}") };
        }
    }
    let code = err.get("code").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let kind = (v.get("type").and_then(Value::as_str) == Some("error"))
        .then(|| err.get("type").and_then(Value::as_str).map(str::to_string))
        .flatten();
    (truncate(&message, 2000), code, kind)
}

static OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)context[ _-]?(length|window)|maximum context|prompt is too long|prompt too long|",
        r"input (is )?too long|too many (input )?tokens|exceeds? the (maximum|max) (number of )?(input )?tokens|",
        r"maximum (number of )?tokens allowed|reduce the length of the (messages|prompt|input)|",
        r"input token count .* exceeds|tokens? exceeds? (the )?(model'?s? )?(limit|maximum)|",
        r"longer than the model'?s? (context|maximum)|context_length_exceeded|exceeded model token limit"
    ))
    .unwrap()
});

pub fn is_context_overflow(message: &str, code: Option<&str>) -> bool {
    code.is_some_and(|c| c.contains("context_length")) || OVERFLOW.is_match(message)
}

static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d[\d,]*").unwrap());
static LIMIT_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?:maximum context length is|maximum number of tokens allowed \(?|context (?:window|length|limit) ",
        r"(?:is |of )?|token limit (?:is |of )?|limit of|max(?:imum)? input (?:tokens|length) (?:is |of )?)\s*(\d[\d,]*)"
    ))
    .unwrap()
});
static ACTUAL_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:resulted in|requested|input token count \(?|prompt (?:has|contains|is)|sent|provided|passed|got)\s*(\d[\d,]*)")
        .unwrap()
});

fn parse_number(s: &str) -> Option<u64> {
    s.replace(',', "").parse().ok()
}

fn captured(re: &Regex, text: &str) -> Option<u64> {
    re.captures(text).and_then(|c| c.get(1)).and_then(|m| parse_number(m.as_str()))
}

/// Rewrites a provider's context error into Anthropic's wording. Claude Desktop looks for
/// "prompt is too long" and reads "<n> tokens > <max> maximum" to decide how much to compact.
pub fn prompt_too_long(message: &str) -> String {
    if message.trim_start().to_ascii_lowercase().starts_with("prompt is too long") {
        return message.to_string();
    }
    let (limit, actual) = match (captured(&LIMIT_NUMBER, message), captured(&ACTUAL_NUMBER, message)) {
        (Some(l), Some(a)) => (Some(l), Some(a)),
        // No wording we know: two distinct large numbers are the size and the limit.
        _ => {
            let mut numbers: Vec<u64> =
                NUMBER.find_iter(message).filter_map(|m| parse_number(m.as_str())).filter(|n| *n >= 1000).collect();
            numbers.sort_unstable();
            numbers.dedup();
            if numbers.len() == 2 {
                (Some(numbers[0]), Some(numbers[1]))
            } else {
                (None, None)
            }
        }
    };
    match (actual, limit) {
        (Some(actual), Some(limit)) if actual > limit => {
            format!("prompt is too long: {actual} tokens > {limit} maximum ({message})")
        }
        _ => format!("prompt is too long: {message}"),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Human-readable network failure without URL query strings or secrets.
pub fn describe_reqwest(e: &reqwest::Error) -> String {
    use crate::i18n::t;
    let what = if e.is_timeout() {
        t("таймаут", "timeout")
    } else if e.is_connect() {
        t("не удалось подключиться", "connection failed")
    } else if e.is_body() || e.is_decode() {
        t("ошибка чтения ответа", "error reading the response")
    } else {
        t("сетевая ошибка", "network error")
    };
    let mut detail = String::new();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        detail = s.to_string();
        source = s.source();
    }
    if detail.is_empty() {
        what.to_string()
    } else {
        format!("{what} ({})", truncate(&detail, 300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_context_error_becomes_prompt_too_long() {
        let body = br#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130500 tokens. Please reduce the length of the messages.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#;
        let e = normalize_upstream_error(400, body);
        assert_eq!(e.status, 400);
        assert_eq!(e.kind, "invalid_request_error");
        assert!(e.message.starts_with("prompt is too long: 130500 tokens > 128000 maximum"), "{}", e.message);
        assert_eq!(e.body()["type"], "error");
        assert_eq!(e.body()["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn deepseek_gemini_and_413_context_errors() {
        let ds = br#"{"error":{"message":"This model's maximum context length is 131072 tokens. However, you requested 140,000 tokens (130000 in the messages, 10000 in the completion). Please reduce the length of the messages or completion.","type":"invalid_request_error"}}"#;
        let e = normalize_upstream_error(400, ds);
        assert!(e.message.starts_with("prompt is too long: 140000 tokens > 131072 maximum"), "{}", e.message);
        let gemini = br#"[{"error":{"code":400,"message":"The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).","status":"INVALID_ARGUMENT"}}]"#;
        let e = normalize_upstream_error(400, gemini);
        assert!(e.message.starts_with("prompt is too long: 1200000 tokens > 1048576 maximum"), "{}", e.message);
        let e = normalize_upstream_error(413, br#"{"error":{"message":"Request too large: prompt is too long for model"}}"#);
        assert_eq!((e.status, e.kind.as_str()), (400, "invalid_request_error"));
    }

    #[test]
    fn other_errors_keep_status_and_get_anthropic_type() {
        let e = normalize_upstream_error(401, br#"{"error":{"message":"Invalid API key","code":"invalid_api_key"}}"#);
        assert_eq!((e.status, e.kind.as_str(), e.message.as_str()), (401, "authentication_error", "Invalid API key"));
        let e = normalize_upstream_error(429, br#"{"error":{"message":"Rate limit: too many tokens per minute"}}"#);
        assert_eq!((e.status, e.kind.as_str()), (429, "rate_limit_error"), "429 is never a context error");
        let e = normalize_upstream_error(402, r#"{"error":{"code":"insufficient_funds","message":"Недостаточно средств"}}"#.as_bytes());
        assert_eq!(e.kind, "billing_error");
        let e = normalize_upstream_error(502, b"<html>Bad Gateway</html>");
        assert_eq!((e.status, e.kind.as_str(), e.message.as_str()), (502, "api_error", "<html>Bad Gateway</html>"));
        let e = normalize_upstream_error(503, b"");
        assert_eq!(e.kind, "overloaded_error");
        assert_eq!(e.message, "upstream returned HTTP 503");
    }

    #[test]
    fn anthropic_errors_pass_through() {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert_eq!(normalize_upstream_error(529, body), ApiError::new(529, "overloaded_error", "Overloaded"));
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        let e = normalize_upstream_error(400, body);
        assert_eq!(e.message, "prompt is too long: 210000 tokens > 200000 maximum");
    }

    #[test]
    fn max_tokens_error_is_not_a_context_error() {
        let e = normalize_upstream_error(
            400,
            br#"{"error":{"message":"max_tokens is too large: 64000. This model supports at most 8192 completion tokens."}}"#,
        );
        assert!(!e.message.starts_with("prompt is too long"));
    }

    #[test]
    fn stream_errors_and_openrouter_raw() {
        let e = normalize_stream_error(&json!({"error": {"code": 502, "message": "Provider returned error", "metadata": {"raw": "upstream died"}}}));
        assert_eq!((e.status, e.kind.as_str()), (502, "api_error"));
        assert_eq!(e.message, "Provider returned error: upstream died");
        let e = normalize_stream_error(&json!({"error": {"message": "context_length_exceeded"}}));
        assert!(e.message.starts_with("prompt is too long"));
    }
}
