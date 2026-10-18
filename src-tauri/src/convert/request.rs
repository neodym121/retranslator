//! Anthropic Messages request → OpenAI chat completions request.

use serde_json::{json, Map, Value};

use super::anthropic::{Block, Content, MediaSource, MessagesRequest, Role, Tool, ToolChoice, ToolResultContent};
use super::{sanitize_tool_id, tool_names, upstream_tool_name, ToolNames, SIGNATURE_PREFIX};
use crate::providers::Quirks;

#[derive(Debug, Clone, Default)]
pub struct OpenAiOptions {
    pub model: String,
    pub stream: bool,
    pub max_tokens_cap: Option<u64>,
    pub quirks: Quirks,
}

/// OpenAI allows at most four stop sequences.
const MAX_STOP: usize = 4;

pub fn to_openai(req: &MessagesRequest, opts: &OpenAiOptions) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        let text = system.text();
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }
    for (i, msg) in req.messages.iter().enumerate() {
        match msg.role {
            Role::User => push_user(&mut messages, &msg.content),
            Role::Assistant => push_assistant(&mut messages, &msg.content),
            Role::System => {
                let cleared = msg.clear_at.as_deref() == Some("next_user_message")
                    && req.messages[i + 1..].iter().any(|m| m.role == Role::User);
                if !cleared {
                    push_system_notice(&mut messages, &msg.content);
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(opts.model));
    body.insert("messages".into(), Value::Array(messages));

    let max_tokens = match (req.max_tokens, opts.max_tokens_cap) {
        (Some(asked), Some(cap)) => Some(asked.min(cap)),
        (asked, cap) => asked.or(cap),
    };
    if let Some(n) = max_tokens {
        let field = if opts.quirks.max_completion_tokens { "max_completion_tokens" } else { "max_tokens" };
        body.insert(field.into(), json!(n));
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(stops) = &req.stop_sequences {
        let stops: Vec<&String> = stops.iter().filter(|s| !s.is_empty()).take(MAX_STOP).collect();
        if !stops.is_empty() {
            body.insert("stop".into(), json!(stops));
        }
    }

    let tools = req.tools.as_deref().map(|t| convert_tools(t, opts.quirks.gemini_schema)).unwrap_or_default();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = &req.tool_choice {
            match choice {
                ToolChoice::Auto { .. } => {}
                ToolChoice::Any { .. } => {
                    body.insert("tool_choice".into(), json!("required"));
                }
                ToolChoice::Tool { name, .. } => {
                    let name = upstream_tool_name(name);
                    body.insert("tool_choice".into(), json!({ "type": "function", "function": { "name": name } }));
                }
                ToolChoice::NoTools => {
                    body.insert("tool_choice".into(), json!("none"));
                }
            }
            if choice.disable_parallel() && !opts.quirks.gemini_schema {
                body.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }

    if opts.stream {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    Value::Object(body)
}

/// Names in the request that the upstream will see shortened, to map its answers back.
pub fn request_tool_names(req: &MessagesRequest) -> ToolNames {
    let declared = req.tools.iter().flatten().map(|t| t.name.clone());
    let used = req.messages.iter().flat_map(|m| m.content.blocks()).filter_map(|b| match b {
        Block::ToolUse { name, .. } => Some(name),
        _ => None,
    });
    tool_names(declared.chain(used))
}

fn push_user(out: &mut Vec<Value>, content: &Content) {
    let mut parts = Vec::new();
    let mut tool_images = Vec::new();
    for block in content.blocks() {
        match block {
            Block::Text { text } => {
                if !text.is_empty() {
                    parts.push(text_part(&text));
                }
            }
            Block::Image { source } => parts.extend(image_part(&source)),
            Block::Document { source, title } => parts.extend(document_part(&source, title.as_deref())),
            Block::ToolResult { tool_use_id, content, is_error } => {
                let (mut text, images) = flatten_tool_result(content.as_ref());
                if text.is_empty() {
                    text = if images.is_empty() { "(no output)".into() } else { "(see attached image)".into() };
                }
                if is_error == Some(true) {
                    text = format!("Error: {text}");
                }
                // Tool messages must directly follow the assistant tool_calls, so they go first.
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": sanitize_tool_id(&tool_use_id),
                    "content": text,
                }));
                tool_images.extend(images);
            }
            Block::ToolUse { .. } | Block::Thinking { .. } | Block::Other => {}
        }
    }
    // Tool messages cannot carry images: they ride along in the user message that follows.
    parts.extend(tool_images);
    if parts.is_empty() {
        return;
    }
    let content = if parts.iter().all(|p| p["type"] == "text") {
        json!(parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n"))
    } else {
        Value::Array(parts)
    };
    out.push(json!({ "role": "user", "content": content }));
}

fn push_assistant(out: &mut Vec<Value>, content: &Content) {
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut reasoning: Option<(String, String)> = None; // (field, text)
    for block in content.blocks() {
        match block {
            Block::Text { text } => {
                if !text.is_empty() {
                    texts.push(text);
                }
            }
            Block::ToolUse { id, name, input } => {
                let args = if input.is_null() { "{}".to_string() } else { input.to_string() };
                tool_calls.push(json!({
                    "id": sanitize_tool_id(&id),
                    "type": "function",
                    "function": { "name": upstream_tool_name(&name), "arguments": args },
                }));
            }
            Block::Thinking { thinking, signature } => {
                // Only reasoning that came from an OpenAI-style upstream goes back, in its own field.
                if let Some(field) = signature.strip_prefix(SIGNATURE_PREFIX) {
                    if !thinking.is_empty() && is_reasoning_field(field) {
                        match &mut reasoning {
                            Some((_, acc)) => acc.push_str(&thinking),
                            None => reasoning = Some((field.to_string(), thinking)),
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    let text = texts.join("\n\n");
    let content = if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) };
    msg.insert("content".into(), content);
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if let Some((field, text)) = reasoning {
        msg.insert(field, json!(text));
    }
    out.push(Value::Object(msg));
}

/// A mid-conversation system message (harness notices, instructions from that point on). Many
/// OpenAI-style upstreams accept `system` only at the start, so its text joins the user turn it
/// follows, marked the way Claude Code marks such notices. Tool additions and removals are dropped.
fn push_system_notice(out: &mut Vec<Value>, content: &Content) {
    let text = content
        .blocks()
        .into_iter()
        .filter_map(|b| match b {
            Block::Text { text } if !text.trim().is_empty() => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() {
        return;
    }
    let notice = format!("<system-reminder>\n{text}\n</system-reminder>");
    if let Some(last) = out.last_mut().filter(|m| m["role"] == "user") {
        match &mut last["content"] {
            Value::String(s) => {
                s.push_str("\n\n");
                s.push_str(&notice);
            }
            Value::Array(parts) => parts.push(text_part(&notice)),
            other => *other = json!(notice),
        }
        return;
    }
    out.push(json!({ "role": "user", "content": notice }));
}

pub fn is_reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning_content" | "reasoning")
}

fn text_part(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn image_part(source: &MediaSource) -> Option<Value> {
    let url = match source {
        MediaSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        MediaSource::Url { url } => url.clone(),
        _ => return Some(text_part("[image omitted: unsupported source]")),
    };
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

fn document_part(source: &MediaSource, title: Option<&str>) -> Option<Value> {
    let header = title.map(|t| format!("[Document: {t}]\n")).unwrap_or_default();
    match source {
        MediaSource::Text { data } => Some(text_part(&format!("{header}{data}"))),
        MediaSource::Base64 { media_type, data } if media_type == "application/pdf" => Some(json!({
            "type": "file",
            "file": {
                "filename": title.map(|t| format!("{t}.pdf")).unwrap_or_else(|| "document.pdf".into()),
                "file_data": format!("data:{media_type};base64,{data}"),
            }
        })),
        MediaSource::Content { content } => {
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            Some(text_part(&format!("{header}{text}")))
        }
        MediaSource::Url { url } => Some(text_part(&format!("{header}[document: {url}]"))),
        _ => Some(text_part(&format!("{header}[document omitted: unsupported source]"))),
    }
}

/// Tool result → (text for the tool message, image parts).
fn flatten_tool_result(content: Option<&ToolResultContent>) -> (String, Vec<Value>) {
    match content {
        None => (String::new(), Vec::new()),
        Some(ToolResultContent::Text(s)) => (s.clone(), Vec::new()),
        Some(ToolResultContent::Blocks(blocks)) => {
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for b in blocks {
                match b {
                    Block::Text { text } => texts.push(text.clone()),
                    Block::Image { source } => images.extend(image_part(source)),
                    Block::Document { source, title } => {
                        if let Some(part) = document_part(source, title.as_deref()) {
                            match part["text"].as_str() {
                                Some(t) => texts.push(t.to_string()),
                                None => images.push(part),
                            }
                        }
                    }
                    _ => {}
                }
            }
            (texts.join("\n"), images)
        }
    }
}

fn convert_tools(tools: &[Tool], gemini: bool) -> Vec<Value> {
    tools
        .iter()
        // Server tools (web_search_*, bash_*, …) have no input_schema and cannot run elsewhere.
        .filter(|t| !t.name.is_empty() && t.input_schema.is_some() && matches!(t.kind.as_deref(), None | Some("custom")))
        .map(|t| {
            let mut schema = t.input_schema.clone().unwrap_or_else(|| json!({ "type": "object" }));
            if let Some(obj) = schema.as_object_mut() {
                obj.remove("$schema");
            }
            let mut function = Map::new();
            function.insert("name".into(), json!(upstream_tool_name(&t.name)));
            if let Some(d) = t.description.as_deref().filter(|d| !d.is_empty()) {
                function.insert("description".into(), json!(d));
            }
            if gemini {
                schema = gemini_schema(&schema);
                let empty_object = schema.get("properties").and_then(Value::as_object).is_none_or(|p| p.is_empty());
                if !empty_object {
                    function.insert("parameters".into(), schema);
                }
            } else {
                function.insert("parameters".into(), schema);
            }
            json!({ "type": "function", "function": function })
        })
        .collect()
}

/// Gemini's OpenAI endpoint accepts an OpenAPI 3.0 subset of JSON Schema; anything else fails the call.
fn gemini_schema(schema: &Value) -> Value {
    const KEEP: &[&str] = &[
        "type", "format", "description", "nullable", "enum", "items", "properties", "required", "minItems",
        "maxItems", "minimum", "maximum", "minLength", "maxLength", "pattern", "anyOf", "title",
    ];
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (key, value) in obj {
        match key.as_str() {
            "const" => {
                out.insert("enum".into(), json!([value]));
            }
            "oneOf" | "anyOf" => {
                if let Some(items) = value.as_array() {
                    out.insert("anyOf".into(), Value::Array(items.iter().map(gemini_schema).collect()));
                }
            }
            "type" => match value {
                Value::Array(types) => {
                    let non_null: Vec<&Value> = types.iter().filter(|t| t.as_str() != Some("null")).collect();
                    if non_null.len() < types.len() {
                        out.insert("nullable".into(), json!(true));
                    }
                    if let Some(t) = non_null.first() {
                        out.insert("type".into(), (*t).clone());
                    }
                }
                other => {
                    out.insert("type".into(), other.clone());
                }
            },
            "format" => {
                if matches!(value.as_str(), Some("enum" | "date-time" | "int32" | "int64" | "float" | "double")) {
                    out.insert("format".into(), value.clone());
                }
            }
            "properties" => {
                if let Some(props) = value.as_object() {
                    if !props.is_empty() {
                        let mapped = props.iter().map(|(k, v)| (k.clone(), gemini_schema(v))).collect();
                        out.insert("properties".into(), Value::Object(mapped));
                    }
                }
            }
            "items" => {
                out.insert("items".into(), gemini_schema(value));
            }
            k if KEEP.contains(&k) => {
                out.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    // `required` may only name properties that exist.
    if let Some(Value::Array(req)) = out.get("required").cloned() {
        let props = out.get("properties").and_then(Value::as_object);
        let kept: Vec<Value> = req
            .into_iter()
            .filter(|r| r.as_str().is_some_and(|name| props.is_some_and(|p| p.contains_key(name))))
            .collect();
        if kept.is_empty() {
            out.remove("required");
        } else {
            out.insert("required".into(), Value::Array(kept));
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(req: Value, opts: OpenAiOptions) -> Value {
        let req: MessagesRequest = serde_json::from_value(req).unwrap();
        to_openai(&req, &opts)
    }

    fn opts() -> OpenAiOptions {
        OpenAiOptions { model: "deepseek-v4.1-flash".into(), ..Default::default() }
    }

    #[test]
    fn system_text_params_and_model() {
        let out = convert(
            json!({
                "model": "claude-sonnet-5.5",
                "system": [{"type": "text", "text": "Be brief", "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "Hi", "cache_control": {"type": "ephemeral"}}]}],
                "max_tokens": 64000, "temperature": 0.2, "top_p": 0.9, "top_k": 5,
                "stop_sequences": ["a", "b", "c", "d", "e"]
            }),
            OpenAiOptions { max_tokens_cap: Some(8192), ..opts() },
        );
        assert_eq!(out["model"], "deepseek-v4.1-flash");
        assert_eq!(out["messages"][0], json!({"role": "system", "content": "Be brief"}));
        assert_eq!(out["messages"][1], json!({"role": "user", "content": "Hi"}));
        assert_eq!(out["max_tokens"], 8192);
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["top_p"], 0.9);
        assert!(out.get("top_k").is_none());
        assert_eq!(out["stop"], json!(["a", "b", "c", "d"]));
        assert!(out.get("stream").is_none());
        // cache_control never reaches an OpenAI-style upstream
        assert!(!out.to_string().contains("cache_control"));
    }

    #[test]
    fn system_as_plain_string_and_stream_options() {
        let out = convert(
            json!({"model": "m", "system": "S", "messages": [{"role": "user", "content": "x"}]}),
            OpenAiOptions { stream: true, quirks: Quirks { max_completion_tokens: true, ..Default::default() }, ..opts() },
        );
        assert_eq!(out["messages"][0]["content"], "S");
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
        assert!(out.get("max_tokens").is_none() && out.get("max_completion_tokens").is_none());
    }

    #[test]
    fn images_become_image_url_parts() {
        let out = convert(
            json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "source": {"type": "url", "url": "https://x.test/a.jpg"}}
            ]}]}),
            OpenAiOptions { quirks: Quirks { max_completion_tokens: true, ..Default::default() }, ..opts() },
        );
        let content = &out["messages"][0]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "What is this?"}));
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(content[2]["image_url"]["url"], "https://x.test/a.jpg");
        assert_eq!(out["max_completion_tokens"], 10);
    }

    #[test]
    fn tool_use_and_tool_result_roundtrip() {
        let out = convert(
            json!({"model": "m", "messages": [
                {"role": "user", "content": "Weather in Paris and Rome?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "get_weather", "input": {"city": "Rome"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "18C"},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "is_error": true,
                     "content": [{"type": "text", "text": "timeout"}]},
                    {"type": "text", "text": "Summarize."}
                ]}
            ]}),
            opts(),
        );
        let m = out["messages"].as_array().unwrap();
        assert_eq!(m.len(), 5);
        assert_eq!(m[1]["role"], "assistant");
        assert_eq!(m[1]["content"], "Checking.");
        let calls = m[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(
            serde_json::from_str::<Value>(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({"city": "Rome"})
        );
        assert_eq!(m[2], json!({"role": "tool", "tool_call_id": "toolu_1", "content": "18C"}));
        assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "toolu_2", "content": "Error: timeout"}));
        assert_eq!(m[4], json!({"role": "user", "content": "Summarize."}));
    }

    #[test]
    fn tool_only_assistant_has_null_content_and_tool_images_follow() {
        let out = convert(
            json!({"model": "m", "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "a.b", "name": "shot", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a.b", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QQ=="}}
                ]}]}
            ]}),
            opts(),
        );
        let m = out["messages"].as_array().unwrap();
        assert!(m[0]["content"].is_null());
        assert_eq!(m[0]["tool_calls"][0]["id"], "a_b");
        assert_eq!(m[0]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(m[1]["tool_call_id"], "a_b");
        assert_eq!(m[1]["content"], "(see attached image)");
        assert_eq!(m[2]["role"], "user");
        assert_eq!(m[2]["content"][0]["image_url"]["url"], "data:image/jpeg;base64,QQ==");
    }

    #[test]
    fn tools_and_tool_choice() {
        let base = json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": [
            {"name": "get_weather", "description": "Weather", "input_schema": {
                "$schema": "http://json-schema.org/draft-07/schema#", "type": "object",
                "properties": {"city": {"type": "string"}}, "required": ["city"]}},
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 3}
        ]});
        let mut req = base.clone();
        req["tool_choice"] = json!({"type": "tool", "name": "get_weather", "disable_parallel_tool_use": true});
        let out = convert(req, opts());
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "server tools are dropped");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["description"], "Weather");
        assert!(tools[0]["function"]["parameters"].get("$schema").is_none());
        assert_eq!(out["tool_choice"], json!({"type": "function", "function": {"name": "get_weather"}}));
        assert_eq!(out["parallel_tool_calls"], false);

        let mut req = base.clone();
        req["tool_choice"] = json!({"type": "any"});
        assert_eq!(convert(req, opts())["tool_choice"], "required");
        let mut req = base.clone();
        req["tool_choice"] = json!({"type": "none"});
        assert_eq!(convert(req, opts())["tool_choice"], "none");
        let mut req = base;
        req["tool_choice"] = json!({"type": "auto"});
        assert!(convert(req, opts()).get("tool_choice").is_none());
    }

    #[test]
    fn gemini_schema_is_reduced_to_openapi_subset() {
        let out = convert(
            json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": [
                {"name": "t", "input_schema": {"type": "object", "additionalProperties": false,
                    "properties": {
                        "mode": {"const": "fast"},
                        "url": {"type": ["string", "null"], "format": "uri"},
                        "tags": {"type": "array", "items": {"type": "string", "examples": ["a"]}}
                    },
                    "required": ["mode", "ghost"]}},
                {"name": "noargs", "input_schema": {"type": "object", "properties": {}}}
            ]}),
            OpenAiOptions { quirks: Quirks { gemini_schema: true, ..Default::default() }, ..opts() },
        );
        let p = &out["tools"][0]["function"]["parameters"];
        assert!(p.get("additionalProperties").is_none());
        assert_eq!(p["properties"]["mode"], json!({"enum": ["fast"]}));
        assert_eq!(p["properties"]["url"], json!({"type": "string", "nullable": true}));
        assert_eq!(p["properties"]["tags"]["items"], json!({"type": "string"}));
        assert_eq!(p["required"], json!(["mode"]));
        assert!(out["tools"][1]["function"].get("parameters").is_none());
    }

    #[test]
    fn reasoning_goes_back_only_from_our_thinking_blocks() {
        let out = convert(
            json!({"model": "m", "messages": [
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "step 1", "signature": "rt:reasoning_content"},
                    {"type": "tool_use", "id": "c1", "name": "f", "input": {"a": 1}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1", "content": "ok"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "anthropic-signed", "signature": "EqQBCkgIARAB..."},
                    {"type": "text", "text": "done"}
                ]}
            ]}),
            opts(),
        );
        let m = out["messages"].as_array().unwrap();
        assert_eq!(m[1]["reasoning_content"], "step 1");
        assert!(m[3].get("reasoning_content").is_none() && m[3].get("reasoning").is_none());
        assert_eq!(m[3]["content"], "done");
    }

    #[test]
    fn long_tool_names_are_shortened_everywhere() {
        let long = "mcp__6f616b42-0ed8-571e-823f-ee4aca6b7ce9__show_widget_with_a_long_name";
        let req: MessagesRequest = serde_json::from_value(json!({"model": "m",
            "tools": [{"name": long, "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}}}],
            "tool_choice": {"type": "tool", "name": long},
            "messages": [{"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": long, "input": {}}]}]
        }))
        .unwrap();
        let out = to_openai(&req, &opts());
        let short = upstream_tool_name(long);
        assert!(short.len() <= 64);
        assert_eq!(out["tools"][0]["function"]["name"], short.as_str());
        assert_eq!(out["tool_choice"]["function"]["name"], short.as_str());
        assert_eq!(out["messages"][0]["tool_calls"][0]["function"]["name"], short.as_str());
        assert_eq!(request_tool_names(&req).get(&short).map(String::as_str), Some(long));
    }

    #[test]
    fn mid_conversation_system_messages_join_the_user_turn() {
        let out = convert(
            json!({"model": "m", "system": "S", "messages": [
                {"role": "user", "content": "Fix the tests"},
                {"role": "system", "content": [{"type": "text", "text": "Be careful", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "run", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
                {"role": "system", "content": "The user also asked for a changelog."},
                {"role": "system", "clear_at": "next_user_message", "content": "Plan first."},
                {"role": "system", "content": [{"type": "tool_removal", "tool": {"type": "tool_reference", "name": "run"}}]},
                {"role": "assistant", "content": "Done."},
                {"role": "user", "content": "Thanks"},
                {"role": "system", "clear_at": "next_user_message", "content": "Answer briefly."}
            ]}),
            opts(),
        );
        let m = out["messages"].as_array().unwrap();
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user", "assistant", "user"]);
        assert_eq!(m[1]["content"], "Fix the tests\n\n<system-reminder>\nBe careful\n</system-reminder>");
        assert_eq!(m[4]["content"], "<system-reminder>\nThe user also asked for a changelog.\n</system-reminder>");
        assert_eq!(m[6]["content"], "Thanks\n\n<system-reminder>\nAnswer briefly.\n</system-reminder>");
        assert!(!out.to_string().contains("Plan first"), "a turn-scoped notice is cleared by the next user turn");
    }

    #[test]
    fn documents() {
        let out = convert(
            json!({"model": "m", "messages": [{"role": "user", "content": [
                {"type": "document", "title": "notes", "source": {"type": "text", "media_type": "text/plain", "data": "hello"}},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBE"}}
            ]}]}),
            opts(),
        );
        let c = &out["messages"][0]["content"];
        assert_eq!(c[0]["text"], "[Document: notes]\nhello");
        assert_eq!(c[1]["type"], "file");
        assert_eq!(c[1]["file"]["file_data"], "data:application/pdf;base64,JVBE");
    }
}
