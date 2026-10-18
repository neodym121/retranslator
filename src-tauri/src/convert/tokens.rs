//! Rough token estimate for count_tokens and for `message_start` before the real count arrives.

use serde_json::Value;

/// Flat cost of an image or PDF page set; their base64 payload says little about tokens.
const MEDIA_TOKENS: u64 = 1600;

/// About four bytes of UTF-8 per token: right for English, close enough for Cyrillic and code.
pub fn estimate_request_tokens(body: &Value) -> u64 {
    let mut bytes = 0u64;
    let mut media = 0u64;
    for key in ["system", "messages", "tools"] {
        if let Some(v) = body.get(key) {
            walk(v, &mut bytes, &mut media);
        }
    }
    let messages = body.get("messages").and_then(Value::as_array).map_or(0, |m| m.len() as u64);
    bytes.div_ceil(4) + media * MEDIA_TOKENS + messages * 4
}

fn walk(v: &Value, bytes: &mut u64, media: &mut u64) {
    match v {
        Value::String(s) => *bytes += s.len() as u64,
        Value::Array(items) => items.iter().for_each(|i| walk(i, bytes, media)),
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str);
            if matches!(kind, Some("image") | Some("document"))
                && map.get("source").and_then(|s| s.get("type")).and_then(Value::as_str) != Some("text")
            {
                *media += 1;
                return;
            }
            for (key, value) in map {
                // Structural fields cost next to nothing; signatures are opaque.
                if matches!(key.as_str(), "type" | "id" | "tool_use_id" | "signature" | "cache_control") {
                    continue;
                }
                walk(value, bytes, media);
            }
        }
        Value::Number(_) | Value::Bool(_) => *bytes += 2,
        Value::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn estimate_scales_with_text_and_counts_images_flat() {
        let small = estimate_request_tokens(&json!({"messages": [{"role": "user", "content": "hi"}]}));
        let big = estimate_request_tokens(&json!({"messages": [{"role": "user", "content": "x".repeat(4000)}]}));
        assert!(small < 10 && (1000..1100).contains(&big), "{small} {big}");
        let img = estimate_request_tokens(&json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(2_000_000)}}
        ]}]}));
        assert!(img < 2000, "{img}");
    }
}
