//! OpenAI chat completion chunks → Anthropic SSE events, one chunk at a time.

use bytes::Bytes;
use serde_json::{json, Value};

use super::response::{map_finish_reason, map_usage, reasoning_of, text_of, Usage};
use super::sse::event_bytes;
use super::{new_id, original_tool_name, sanitize_tool_id, ToolNames, SIGNATURE_PREFIX};

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub data: Value,
}

impl Event {
    pub fn bytes(&self) -> Bytes {
        event_bytes(self.name, &self.data)
    }
}

/// Identity of an upstream tool call: its `index` and/or `id`, whichever the provider sends.
#[derive(Debug, Clone, Default, PartialEq)]
struct CallKey {
    index: Option<u64>,
    id: Option<String>,
}

impl CallKey {
    /// A fragment continues this call unless it names another id or another index.
    fn matches(&self, index: Option<u64>, id: Option<&str>) -> bool {
        if let (Some(a), Some(b)) = (self.id.as_deref(), id) {
            if a != b {
                return false;
            }
        }
        match (self.index, index) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

#[derive(Debug)]
enum Open {
    Text,
    Thinking { field: &'static str },
    Tool { key: CallKey, sent_args: bool },
}

/// A tool call whose name has not arrived yet; its arguments wait until it has one.
#[derive(Debug)]
struct Pending {
    key: CallKey,
    args: String,
}

#[derive(Debug)]
pub struct StreamConverter {
    message_id: String,
    model: String,
    input_estimate: u64,
    index: u64,
    open: Option<Open>,
    pending: Option<Pending>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    has_tool_use: bool,
    any_block: bool,
    finished: bool,
    tool_names: ToolNames,
}

impl StreamConverter {
    /// `input_estimate` fills `message_start` (the real count only arrives at the end of an OpenAI stream).
    pub fn new(model: &str, input_estimate: u64) -> Self {
        Self {
            message_id: new_id("msg_"),
            model: model.to_string(),
            input_estimate,
            index: 0,
            open: None,
            pending: None,
            finish_reason: None,
            usage: None,
            has_tool_use: false,
            any_block: false,
            finished: false,
            tool_names: ToolNames::new(),
        }
    }

    /// Shortened tool names to restore (see `convert::upstream_tool_name`).
    pub fn with_tool_names(mut self, names: ToolNames) -> Self {
        self.tool_names = names;
        self
    }

    pub fn start(&self) -> Event {
        Event {
            name: "message_start",
            data: json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": Usage { input_tokens: self.input_estimate, ..Default::default() }.to_json(),
                }
            }),
        }
    }

    /// Usage as it will be (or was) reported in `message_delta`.
    pub fn usage(&self) -> Usage {
        self.usage.unwrap_or(Usage { input_tokens: self.input_estimate, ..Default::default() })
    }

    pub fn push(&mut self, chunk: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(map_usage(u));
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return out;
        };
        if let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) {
            if let Some((field, text)) = reasoning_of(delta) {
                self.thinking(field, text, &mut out);
            }
            let text = text_of(delta.get("content"));
            if !text.is_empty() {
                self.text(&text, &mut out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_fragment(call, &mut out);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        out
    }

    /// Closes the message: last block, `message_delta` with stop reason and usage, `message_stop`.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.flush_pending(&mut out);
        self.close(&mut out);
        if !self.any_block {
            self.begin(json!({ "type": "text", "text": "" }), Open::Text, &mut out);
            self.close(&mut out);
        }
        let stop_reason = map_finish_reason(self.finish_reason.as_deref(), self.has_tool_use);
        out.push(Event {
            name: "message_delta",
            data: json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                "usage": self.usage().to_json(),
            }),
        });
        out.push(Event { name: "message_stop", data: json!({ "type": "message_stop" }) });
        self.finished = true;
        out
    }

    fn thinking(&mut self, field: &'static str, text: &str, out: &mut Vec<Event>) {
        if !matches!(self.open, Some(Open::Thinking { .. })) {
            self.flush_pending(out);
            self.begin(json!({ "type": "thinking", "thinking": "", "signature": "" }), Open::Thinking { field }, out);
        }
        self.delta(json!({ "type": "thinking_delta", "thinking": text }), out);
    }

    fn text(&mut self, text: &str, out: &mut Vec<Event>) {
        if !matches!(self.open, Some(Open::Text)) {
            self.flush_pending(out);
            self.begin(json!({ "type": "text", "text": "" }), Open::Text, out);
        }
        self.delta(json!({ "type": "text_delta", "text": text }), out);
    }

    fn tool_fragment(&mut self, call: &Value, out: &mut Vec<Event>) {
        let index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        let function = call.get("function");
        let name = function.and_then(|f| f.get("name")).and_then(Value::as_str).filter(|s| !s.is_empty());
        let args = match function.and_then(|f| f.get("arguments")) {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => String::new(),
            Some(other) => other.to_string(),
        };

        if let Some(Open::Tool { key, .. }) = &self.open {
            if key.matches(index, id) {
                self.tool_args(&args, out);
                return;
            }
        }
        if let Some(pending) = &mut self.pending {
            if pending.key.matches(index, id) {
                pending.args.push_str(&args);
                if pending.key.id.is_none() {
                    pending.key.id = id.map(str::to_string);
                }
                if let Some(name) = name {
                    let pending = self.pending.take().unwrap();
                    self.begin_tool(pending.key, name, out);
                    self.tool_args(&pending.args, out);
                }
                return;
            }
            self.flush_pending(out);
        }
        let key = CallKey { index, id: id.map(str::to_string) };
        match name {
            Some(name) => {
                self.begin_tool(key, name, out);
                self.tool_args(&args, out);
            }
            None => {
                self.close(out);
                self.pending = Some(Pending { key, args });
            }
        }
    }

    fn begin_tool(&mut self, key: CallKey, name: &str, out: &mut Vec<Event>) {
        let id = key.id.as_deref().map(sanitize_tool_id).unwrap_or_else(|| new_id("toolu_"));
        let name = original_tool_name(&self.tool_names, name).to_string();
        self.begin(
            json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
            Open::Tool { key, sent_args: false },
            out,
        );
        self.has_tool_use = true;
    }

    fn tool_args(&mut self, args: &str, out: &mut Vec<Event>) {
        if args.is_empty() {
            return;
        }
        if let Some(Open::Tool { sent_args, .. }) = &mut self.open {
            *sent_args = true;
            self.delta(json!({ "type": "input_json_delta", "partial_json": args }), out);
        }
    }

    /// A call that never got a name still has to reach the client.
    fn flush_pending(&mut self, out: &mut Vec<Event>) {
        if let Some(pending) = self.pending.take() {
            self.begin_tool(pending.key, "unknown_tool", out);
            self.tool_args(&pending.args, out);
        }
    }

    fn begin(&mut self, block: Value, open: Open, out: &mut Vec<Event>) {
        self.close(out);
        out.push(Event {
            name: "content_block_start",
            data: json!({ "type": "content_block_start", "index": self.index, "content_block": block }),
        });
        self.open = Some(open);
        self.any_block = true;
    }

    fn delta(&self, delta: Value, out: &mut Vec<Event>) {
        out.push(Event {
            name: "content_block_delta",
            data: json!({ "type": "content_block_delta", "index": self.index, "delta": delta }),
        });
    }

    fn close(&mut self, out: &mut Vec<Event>) {
        let Some(open) = self.open.take() else { return };
        match open {
            Open::Thinking { field } => {
                self.delta(json!({ "type": "signature_delta", "signature": format!("{SIGNATURE_PREFIX}{field}") }), out)
            }
            // The client parses the accumulated JSON at block stop; an empty buffer is not valid JSON.
            Open::Tool { sent_args: false, .. } => {
                self.delta(json!({ "type": "input_json_delta", "partial_json": "{}" }), out)
            }
            _ => {}
        }
        out.push(Event {
            name: "content_block_stop",
            data: json!({ "type": "content_block_stop", "index": self.index }),
        });
        self.index += 1;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Rebuilds the final message the way an Anthropic SDK client does, checking event order on the way.
    pub(crate) fn accumulate(events: &[Event]) -> Value {
        assert_eq!(events.first().map(|e| e.name), Some("message_start"), "message_start must come first");
        assert_eq!(events.last().map(|e| e.name), Some("message_stop"), "message_stop must come last");
        let mut message = events[0].data["message"].clone();
        let mut content: Vec<Value> = Vec::new();
        let mut json_buf = String::new();
        let mut open: Option<u64> = None;
        for e in &events[1..] {
            assert_eq!(e.data["type"], e.name, "event name and data.type differ");
            match e.name {
                "content_block_start" => {
                    assert!(open.is_none(), "block started while another is open");
                    let idx = e.data["index"].as_u64().unwrap();
                    assert_eq!(idx as usize, content.len(), "block indices must be sequential");
                    content.push(e.data["content_block"].clone());
                    open = Some(idx);
                    json_buf.clear();
                }
                "content_block_delta" => {
                    let idx = e.data["index"].as_u64().unwrap();
                    assert_eq!(open, Some(idx), "delta for a block that is not open");
                    let block = &mut content[idx as usize];
                    let d = &e.data["delta"];
                    match d["type"].as_str().unwrap() {
                        "text_delta" => {
                            let t = block["text"].as_str().unwrap().to_string() + d["text"].as_str().unwrap();
                            block["text"] = json!(t);
                        }
                        "thinking_delta" => {
                            let t = block["thinking"].as_str().unwrap().to_string() + d["thinking"].as_str().unwrap();
                            block["thinking"] = json!(t);
                        }
                        "signature_delta" => block["signature"] = d["signature"].clone(),
                        "input_json_delta" => json_buf.push_str(d["partial_json"].as_str().unwrap()),
                        other => panic!("unexpected delta {other}"),
                    }
                }
                "content_block_stop" => {
                    let idx = e.data["index"].as_u64().unwrap();
                    assert_eq!(open, Some(idx));
                    if content[idx as usize]["type"] == "tool_use" {
                        content[idx as usize]["input"] = serde_json::from_str(&json_buf).expect("tool input is valid JSON");
                    }
                    open = None;
                }
                "message_delta" => {
                    message["stop_reason"] = e.data["delta"]["stop_reason"].clone();
                    message["usage"] = e.data["usage"].clone();
                }
                "message_stop" => assert!(open.is_none(), "message_stop with an open block"),
                other => panic!("unexpected event {other}"),
            }
        }
        message["content"] = Value::Array(content);
        message
    }

    fn run(chunks: &[Value]) -> Value {
        let mut conv = StreamConverter::new("claude-sonnet-5.5", 42);
        let mut events = vec![conv.start()];
        for c in chunks {
            events.extend(conv.push(c));
        }
        events.extend(conv.finish());
        accumulate(&events)
    }

    fn delta(d: Value) -> Value {
        json!({ "choices": [{ "index": 0, "delta": d, "finish_reason": null }] })
    }

    fn finish(reason: &str) -> Value {
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] })
    }

    #[test]
    fn streams_text_and_usage_chunk_after_finish() {
        let msg = run(&[
            delta(json!({"role": "assistant", "content": ""})),
            delta(json!({"content": "Hel"})),
            delta(json!({"content": "lo"})),
            finish("stop"),
            json!({"choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 2}}),
        ]);
        assert_eq!(msg["model"], "claude-sonnet-5.5");
        assert_eq!(msg["content"], json!([{"type": "text", "text": "Hello"}]));
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(
            msg["usage"],
            json!({"input_tokens": 30, "output_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
        );
    }

    #[test]
    fn message_start_carries_estimate_and_all_usage_fields() {
        let conv = StreamConverter::new("m", 42);
        let start = conv.start();
        let usage = &start.data["message"]["usage"];
        assert_eq!(usage["input_tokens"], 42);
        for k in ["output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"] {
            assert_eq!(usage[k], 0);
        }
        // Without upstream usage the estimate stays.
        let msg = run(&[delta(json!({"content": "x"}))]);
        assert_eq!(msg["usage"]["input_tokens"], 42);
    }

    #[test]
    fn reasoning_becomes_thinking_block_before_text() {
        let msg = run(&[
            delta(json!({"reasoning_content": "Let me "})),
            delta(json!({"reasoning_content": "think."})),
            delta(json!({"content": "Answer"})),
            finish("stop"),
        ]);
        assert_eq!(
            msg["content"],
            json!([
                {"type": "thinking", "thinking": "Let me think.", "signature": "rt:reasoning_content"},
                {"type": "text", "text": "Answer"}
            ])
        );
        let msg = run(&[delta(json!({"reasoning": "r"})), delta(json!({"content": "a"}))]);
        assert_eq!(msg["content"][0]["signature"], "rt:reasoning");
    }

    #[test]
    fn tool_call_arguments_stream_in_pieces() {
        let msg = run(&[
            delta(json!({"content": "Checking."})),
            delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": ""}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"ci"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "ty\":\"Paris\"}"}}]})),
            delta(json!({"tool_calls": [{"index": 1, "id": "call_2", "type": "function",
                "function": {"name": "get_time", "arguments": "{\"tz\":"}}]})),
            delta(json!({"tool_calls": [{"index": 1, "function": {"arguments": "\"CET\"}"}}]})),
            finish("tool_calls"),
        ]);
        assert_eq!(
            msg["content"],
            json!([
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Paris"}},
                {"type": "tool_use", "id": "call_2", "name": "get_time", "input": {"tz": "CET"}}
            ])
        );
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    #[test]
    fn whole_tool_calls_in_one_chunk_same_index_different_ids() {
        // Gemini-style: complete calls, sometimes all with index 0.
        let msg = run(&[
            delta(json!({"tool_calls": [
                {"index": 0, "id": "a", "function": {"name": "f", "arguments": "{\"x\":1}"}},
                {"index": 0, "id": "b", "function": {"name": "g", "arguments": "{\"y\":2}"}}
            ]})),
            finish("stop"),
        ]);
        let c = msg["content"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["input"], json!({"x": 1}));
        assert_eq!(c[1]["name"], "g");
        assert_eq!(msg["stop_reason"], "tool_use", "tool calls with finish_reason stop");
    }

    #[test]
    fn tool_without_arguments_and_without_id() {
        let msg = run(&[delta(json!({"tool_calls": [{"function": {"name": "list_files"}}]})), finish("tool_calls")]);
        assert_eq!(msg["content"][0]["input"], json!({}));
        assert!(msg["content"][0]["id"].as_str().unwrap().starts_with("toolu_"));
    }

    #[test]
    fn tool_name_arriving_late_waits_for_it() {
        let msg = run(&[
            delta(json!({"tool_calls": [{"index": 0, "id": "c", "function": {"arguments": "{\"q\":"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"name": "search", "arguments": "\"rust\"}"}}]})),
        ]);
        assert_eq!(msg["content"], json!([{"type": "tool_use", "id": "c", "name": "search", "input": {"q": "rust"}}]));
    }

    #[test]
    fn text_after_tool_and_reasoning_after_text_open_new_blocks() {
        let msg = run(&[
            delta(json!({"reasoning_content": "a"})),
            delta(json!({"tool_calls": [{"index": 0, "id": "t", "function": {"name": "f", "arguments": "{}"}}]})),
            delta(json!({"content": "b"})),
            delta(json!({"reasoning_content": "c"})),
        ]);
        let types: Vec<&str> = msg["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["thinking", "tool_use", "text", "thinking"]);
    }

    #[test]
    fn shortened_tool_names_are_restored() {
        let long = "mcp__1a59c906-04da-521d-bda7-7f71b9f9e01c__reconnect_session_connector".to_string();
        let short = crate::convert::upstream_tool_name(&long);
        let mut conv = StreamConverter::new("m", 0).with_tool_names(crate::convert::tool_names([long.clone()]));
        let mut events = vec![conv.start()];
        events.extend(conv.push(&delta(json!({"tool_calls": [{"index": 0, "id": "c", "function": {"name": short, "arguments": "{}"}}]}))));
        events.extend(conv.finish());
        assert_eq!(accumulate(&events)["content"][0]["name"], long.as_str());
    }

    #[test]
    fn empty_stream_and_length_stop() {
        let msg = run(&[finish("length")]);
        assert_eq!(msg["content"], json!([{"type": "text", "text": ""}]));
        assert_eq!(msg["stop_reason"], "max_tokens");
    }

    #[test]
    fn finish_is_idempotent_and_push_after_finish_is_ignored() {
        let mut conv = StreamConverter::new("m", 0);
        let _ = conv.start();
        assert!(!conv.finish().is_empty());
        assert!(conv.finish().is_empty());
        assert!(conv.push(&delta(json!({"content": "late"}))).is_empty());
    }

    #[test]
    fn event_bytes_are_sse() {
        let conv = StreamConverter::new("m", 0);
        let bytes = conv.start().bytes();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with("event: message_start\ndata: {"));
        assert!(text.ends_with("}\n\n"));
    }
}
