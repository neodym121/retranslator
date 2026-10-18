//! OpenAI chat completion → Anthropic message (non-streaming), plus usage and stop reason mapping.

use serde::Serialize;
use serde_json::{json, Value};

use super::{new_id, original_tool_name, sanitize_tool_id, ToolNames, SIGNATURE_PREFIX};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn to_json(self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_creation_input_tokens": self.cache_creation_input_tokens,
            "cache_read_input_tokens": self.cache_read_input_tokens,
        })
    }

    /// Reads an Anthropic-style usage object (missing fields are zero).
    pub fn from_anthropic(u: &Value) -> Usage {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        Usage {
            input_tokens: n("input_tokens"),
            output_tokens: n("output_tokens"),
            cache_creation_input_tokens: n("cache_creation_input_tokens"),
            cache_read_input_tokens: n("cache_read_input_tokens"),
        }
    }
}

fn num(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_u64().or_else(|| cur.as_f64().map(|f| f.max(0.0) as u64))
}

/// OpenAI usage → Anthropic usage. OpenAI's `prompt_tokens` includes cached tokens while
/// Anthropic's `input_tokens` does not, so cached reads (and writes, where reported) are split out.
pub fn map_usage(u: &Value) -> Usage {
    let prompt = num(u, &["prompt_tokens"]).or_else(|| num(u, &["input_tokens"])).unwrap_or(0);
    let output = num(u, &["completion_tokens"]).or_else(|| num(u, &["output_tokens"])).unwrap_or(0);
    let cache_read = num(u, &["prompt_tokens_details", "cached_tokens"])
        .or_else(|| num(u, &["prompt_cache_hit_tokens"]))
        .or_else(|| num(u, &["cache_read_input_tokens"]))
        .unwrap_or(0);
    let cache_write = num(u, &["prompt_tokens_details", "cache_write_tokens"])
        .or_else(|| num(u, &["cache_creation_input_tokens"]))
        .unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cache_read).saturating_sub(cache_write),
        output_tokens: output,
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cache_read,
    }
}

pub fn map_finish_reason(reason: Option<&str>, has_tool_use: bool) -> &'static str {
    match reason {
        Some("length") | Some("max_tokens") => "max_tokens",
        Some("content_filter") | Some("safety") => "refusal",
        Some("tool_calls") | Some("function_call") => "tool_use",
        // Several providers answer "stop" even when they return tool calls.
        _ if has_tool_use => "tool_use",
        _ => "end_turn",
    }
}

/// Reasoning text and the field it came from (`reasoning_content` for DeepSeek and most
/// OpenAI-compatible servers, `reasoning` for OpenRouter/Polza).
pub fn reasoning_of(v: &Value) -> Option<(&'static str, &str)> {
    for field in ["reasoning_content", "reasoning"] {
        if let Some(text) = v.get(field).and_then(Value::as_str) {
            if !text.is_empty() {
                return Some((field, text));
            }
        }
    }
    None
}

/// Text of `message.content`: a string, or an array of `{type:"text", text}` parts.
pub fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Tool call arguments are a JSON string; the Anthropic side needs an object.
pub fn parse_arguments(args: &str) -> Value {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => json!({ "value": other }),
        Err(_) => json!({ "_raw_arguments": args }),
    }
}

pub fn openai_to_anthropic(resp: &Value, local_model: &str, names: &ToolNames) -> Value {
    let choice = resp.get("choices").and_then(|c| c.get(0)).cloned().unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut content = Vec::new();

    if let Some((field, text)) = reasoning_of(&message) {
        content.push(json!({
            "type": "thinking",
            "thinking": text,
            "signature": format!("{SIGNATURE_PREFIX}{field}"),
        }));
    }
    let text = text_of(message.get("content"));
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }

    let mut calls: Vec<Value> = message.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(fc) = message.get("function_call") {
        calls.push(json!({ "function": fc }));
    }
    let mut has_tool_use = false;
    for call in &calls {
        let Some(function) = call.get("function") else { continue };
        let name = function.get("name").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(sanitize_tool_id)
            .unwrap_or_else(|| new_id("toolu_"));
        let input = match function.get("arguments") {
            Some(Value::String(s)) => parse_arguments(s),
            Some(Value::Object(o)) => Value::Object(o.clone()),
            _ => json!({}),
        };
        let name = original_tool_name(names, name);
        content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
        has_tool_use = true;
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    let finish = choice.get("finish_reason").and_then(Value::as_str);
    let usage = resp.get("usage").map(map_usage).unwrap_or_default();
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("msg_{}", sanitize_tool_id(s)))
        .unwrap_or_else(|| new_id("msg_"));
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": local_model,
        "content": content,
        "stop_reason": map_finish_reason(finish, has_tool_use),
        "stop_sequence": null,
        "usage": usage.to_json(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_response_with_usage() {
        let out = openai_to_anthropic(
            &json!({
                "id": "chatcmpl-1",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 100}}
            }),
            "claude-sonnet-5.5",
            &ToolNames::new(),
        );
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "claude-sonnet-5.5");
        assert_eq!(out["id"], "msg_chatcmpl-1");
        assert_eq!(out["content"], json!([{"type": "text", "text": "Hello"}]));
        assert_eq!(out["stop_reason"], "end_turn");
        assert!(out["stop_sequence"].is_null());
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 20, "output_tokens": 7, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 100})
        );
    }

    #[test]
    fn tool_calls_reasoning_and_stop_reason() {
        let out = openai_to_anthropic(
            &json!({
                "choices": [{"message": {
                    "role": "assistant", "content": null, "reasoning_content": "Need weather.",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "noop", "arguments": ""}}
                    ]
                }, "finish_reason": "stop"}]
            }),
            "claude-x",
            &ToolNames::new(),
        );
        let c = out["content"].as_array().unwrap();
        assert_eq!(c[0], json!({"type": "thinking", "thinking": "Need weather.", "signature": "rt:reasoning_content"}));
        assert_eq!(c[1], json!({"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Paris"}}));
        assert_eq!(c[2]["input"], json!({}));
        assert_eq!(out["stop_reason"], "tool_use");
        assert!(out["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(out["usage"]["input_tokens"], 0);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 0);
    }

    #[test]
    fn finish_reasons() {
        assert_eq!(map_finish_reason(Some("length"), false), "max_tokens");
        assert_eq!(map_finish_reason(Some("tool_calls"), true), "tool_use");
        assert_eq!(map_finish_reason(Some("content_filter"), false), "refusal");
        assert_eq!(map_finish_reason(Some("stop"), false), "end_turn");
        assert_eq!(map_finish_reason(None, false), "end_turn");
    }

    #[test]
    fn deepseek_cache_fields_and_bad_arguments() {
        let u = map_usage(&json!({"prompt_tokens": 50, "completion_tokens": 5, "prompt_cache_hit_tokens": 30, "prompt_cache_miss_tokens": 20}));
        assert_eq!(u, Usage { input_tokens: 20, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 30 });
        assert_eq!(parse_arguments("{bad"), json!({"_raw_arguments": "{bad"}));
        assert_eq!(parse_arguments("[1]"), json!({"value": [1]}));
    }

    #[test]
    fn empty_answer_still_has_a_block() {
        let out = openai_to_anthropic(&json!({"choices": [{"message": {"content": ""}, "finish_reason": "length"}]}), "m", &ToolNames::new());
        assert_eq!(out["content"], json!([{"type": "text", "text": ""}]));
        assert_eq!(out["stop_reason"], "max_tokens");
    }
}
