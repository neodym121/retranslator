//! Anthropic Messages ⇄ OpenAI chat completions conversion.

pub mod anthropic;
pub mod errors;
pub mod request;
pub mod response;
pub mod sse;
pub mod stream;
pub mod tokens;

use std::collections::HashMap;

use rand::distr::{Alphanumeric, SampleString};

/// Thinking blocks made from upstream reasoning carry a signature `rt:<field>`, where `<field>` is
/// the OpenAI-side field the reasoning came from. It lets the converter hand the reasoning back
/// in the same field on the next turn and drop thinking blocks that came from somewhere else.
pub const SIGNATURE_PREFIX: &str = "rt:";

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", Alphanumeric.sample_string(&mut rand::rng(), 24))
}

/// Anthropic tool ids must match `^[a-zA-Z0-9_-]+$`. The same mapping is applied to tool_use and
/// tool_result ids, so pairs stay consistent.
pub fn sanitize_tool_id(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if clean.is_empty() {
        new_id("toolu_")
    } else {
        clean
    }
}

/// Upstream tool name → the client's original name, for names that had to be shortened.
pub type ToolNames = HashMap<String, String>;

const MAX_TOOL_NAME: usize = 64;

fn valid_tool_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// OpenAI and Gemini cap function names at 64 characters of `[a-zA-Z0-9_-]`, while Claude allows 128
/// (MCP tools like `mcp__<uuid>__name` go past 64). Longer or odd names get a stable short form:
/// the same input always gives the same output, so history and new calls agree.
pub fn upstream_tool_name(name: &str) -> String {
    if !name.is_empty() && name.len() <= MAX_TOOL_NAME && name.chars().all(valid_tool_char) {
        return name.to_string();
    }
    // FNV-1a: tiny, deterministic across runs and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let head: String = name.chars().map(|c| if valid_tool_char(c) { c } else { '_' }).take(50).collect();
    format!("{head}_{:012x}", hash & 0xffff_ffff_ffff)
}

/// Reverse map for every name in the request that was shortened.
pub fn tool_names(names: impl IntoIterator<Item = String>) -> ToolNames {
    names
        .into_iter()
        .filter_map(|n| {
            let up = upstream_tool_name(&n);
            (up != n).then_some((up, n))
        })
        .collect()
}

pub fn original_tool_name<'a>(names: &'a ToolNames, upstream: &'a str) -> &'a str {
    names.get(upstream).map(String::as_str).unwrap_or(upstream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_tool_names_are_shortened_stably() {
        let long = "mcp__1a59c906-04da-521d-bda7-7f71b9f9e01c__reconnect_session_connector";
        let short = upstream_tool_name(long);
        assert!(short.len() <= 64 && short.chars().all(valid_tool_char), "{short}");
        assert_eq!(short, upstream_tool_name(long));
        assert_ne!(short, upstream_tool_name(&format!("{long}2")));
        assert_eq!(upstream_tool_name("get_weather"), "get_weather");
        let map = tool_names([long.to_string(), "get_weather".to_string()]);
        assert_eq!(map.len(), 1);
        assert_eq!(original_tool_name(&map, &short), long);
        assert_eq!(original_tool_name(&map, "get_weather"), "get_weather");
    }

    #[test]
    fn tool_ids_are_sanitized_stably() {
        assert_eq!(sanitize_tool_id("call_abc-1"), "call_abc-1");
        assert_eq!(sanitize_tool_id("functions.get:0"), "functions_get_0");
        assert!(sanitize_tool_id("").starts_with("toolu_"));
    }
}
