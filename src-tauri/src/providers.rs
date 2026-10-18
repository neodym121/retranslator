//! Upstream providers (always OpenAI-compatible): presets, endpoint URLs, auth headers,
//! model listing and connection checks.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::i18n::t;

pub const CUSTOM_PRESET: &str = "custom";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Provider {
    pub id: String,
    pub name: String,
    /// Preset key it was created from (`openai`, `gemini`, …, `custom`).
    pub preset: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    pub key: &'static str,
    pub name: &'static str,
    /// OpenAI-compatible base URL, as for the OpenAI SDK (`…/v1`). Empty for a custom provider.
    pub base_url: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset { key: "openai", name: "OpenAI", base_url: "https://api.openai.com/v1" },
    Preset { key: "gemini", name: "Google Gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai" },
    Preset { key: "openrouter", name: "OpenRouter", base_url: "https://openrouter.ai/api/v1" },
    Preset { key: "polza", name: "PolzaAI", base_url: "https://polza.ai/api/v1" },
    Preset { key: CUSTOM_PRESET, name: "Custom", base_url: "" },
];

/// A preset with a fixed name and URL; `None` for custom providers.
pub fn fixed_preset(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.key == key && p.key != CUSTOM_PRESET)
}

/// Preset providers always carry the preset's name and URL: only the key is the user's.
pub fn apply_preset(provider: &mut Provider) {
    match fixed_preset(&provider.preset) {
        Some(preset) => {
            provider.name = preset.name.to_string();
            provider.base_url = preset.base_url.to_string();
        }
        None => provider.preset = CUSTOM_PRESET.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    ChatCompletions,
    Models,
}

/// Builds an upstream URL. Bases include the version (`…/v1`) like the OpenAI SDK expects;
/// a pasted full endpoint path is trimmed.
pub fn endpoint_url(base: &str, endpoint: Endpoint) -> String {
    let mut base = base.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/models"] {
        if base.ends_with(suffix) {
            base.truncate(base.len() - suffix.len());
        }
    }
    let leaf = match endpoint {
        Endpoint::ChatCompletions => "chat/completions",
        Endpoint::Models => "models",
    };
    format!("{}/{leaf}", base.trim_end_matches('/'))
}

pub fn host_of(base: &str) -> String {
    reqwest::Url::parse(base.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// Upstream-specific quirks derived from the base URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quirks {
    /// api.openai.com wants `max_completion_tokens` for current models.
    pub max_completion_tokens: bool,
    /// Gemini's OpenAI endpoint accepts only an OpenAPI subset of JSON Schema in tools.
    pub gemini_schema: bool,
    /// Gemini lists models as `models/<id>`.
    pub strip_models_prefix: bool,
    pub openrouter: bool,
}

pub fn quirks(base: &str) -> Quirks {
    let host = host_of(base);
    Quirks {
        max_completion_tokens: host == "api.openai.com",
        gemini_schema: host == "generativelanguage.googleapis.com",
        strip_models_prefix: host == "generativelanguage.googleapis.com",
        openrouter: host.ends_with("openrouter.ai"),
    }
}

/// `Authorization: Bearer <key>` plus provider extras.
pub fn upstream_headers(provider: &Provider, api_key: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let key = api_key.trim();
    if !key.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    if quirks(&provider.base_url).openrouter {
        headers.insert(HeaderName::from_static("x-title"), HeaderValue::from_static("Retranslator"));
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static("https://github.com/neodym121/retranslator"),
        );
    }
    headers
}

/// Pulls model ids out of a `/models` response: `{data:[{id}]}`, `{models:[…]}` or a bare array.
pub fn parse_model_ids(body: &Value, strip_models_prefix: bool) -> Vec<String> {
    let list = body
        .get("data")
        .or_else(|| body.get("models"))
        .unwrap_or(body)
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = list
        .iter()
        .filter_map(|m| match m {
            Value::String(s) => Some(s.clone()),
            other => other
                .get("id")
                .or_else(|| other.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .map(|id| {
            if strip_models_prefix {
                id.strip_prefix("models/").map(str::to_string).unwrap_or(id)
            } else {
                id
            }
        })
        .filter(|id| !id.trim().is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelList {
    pub models: Vec<String>,
    pub status: u16,
    pub latency_ms: u64,
}

/// `GET /models` at the provider.
pub async fn fetch_models(
    http: &reqwest::Client,
    provider: &Provider,
    api_key: &str,
) -> Result<ModelList, String> {
    if provider.base_url.trim().is_empty() {
        return Err(t("Не указан base URL", "Base URL is missing").into());
    }
    let url = endpoint_url(&provider.base_url, Endpoint::Models);
    let started = Instant::now();
    let resp = http
        .get(&url)
        .headers(upstream_headers(provider, api_key))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| {
            let why = crate::convert::errors::describe_reqwest(&e);
            t(format!("Нет соединения с {url}: {why}"), format!("Cannot reach {url}: {why}"))
        })?;
    let status = resp.status().as_u16();
    let latency_ms = started.elapsed().as_millis() as u64;
    let bytes = resp.bytes().await.map_err(|e| t(format!("Ответ оборвался: {e}"), format!("Response cut off: {e}")))?;
    if !(200..300).contains(&status) {
        let err = crate::convert::errors::normalize_upstream_error(status, &bytes);
        return Err(format!("HTTP {status}: {}", err.message));
    }
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|_| t("Ответ /models не JSON", "The /models response is not JSON").to_string())?;
    Ok(ModelList {
        models: parse_model_ids(&body, quirks(&provider.base_url).strip_models_prefix),
        status,
        latency_ms,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionCheck {
    pub ok: bool,
    pub message: String,
    pub status: Option<u16>,
    pub latency_ms: Option<u64>,
    /// Model ids from `/models`, so a passed check also fills the suggestions.
    pub models: Option<Vec<String>>,
}

/// Connection check: a `/models` call; auth errors fail, a missing `/models` route passes with a note.
pub async fn check_connection(http: &reqwest::Client, provider: &Provider, api_key: &str) -> ConnectionCheck {
    match fetch_models(http, provider, api_key).await {
        Ok(list) => ConnectionCheck {
            ok: true,
            message: t(format!("Соединение есть, моделей: {}", list.models.len()), format!("Connected, models: {}", list.models.len())),
            status: Some(list.status),
            latency_ms: Some(list.latency_ms),
            models: Some(list.models),
        },
        Err(message) => {
            let status = message
                .strip_prefix("HTTP ")
                .and_then(|s| s.get(..3))
                .and_then(|s| s.parse::<u16>().ok());
            let route_missing = matches!(status, Some(404) | Some(405));
            ConnectionCheck {
                ok: route_missing,
                message: if route_missing {
                    t(
                        "Сервер отвечает, но не отдаёт список моделей (/models). Реальный id впишите вручную.",
                        "The server answers but has no model list (/models). Type the real id by hand.",
                    )
                    .into()
                } else {
                    message
                },
                status,
                latency_ms: None,
                models: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_urls_follow_openai_sdk_conventions() {
        use Endpoint::*;
        assert_eq!(endpoint_url("https://api.openai.com/v1", ChatCompletions), "https://api.openai.com/v1/chat/completions");
        assert_eq!(endpoint_url("https://api.openai.com/v1/", Models), "https://api.openai.com/v1/models");
        assert_eq!(endpoint_url("https://api.deepseek.com", ChatCompletions), "https://api.deepseek.com/chat/completions");
        assert_eq!(
            endpoint_url("https://polza.ai/api/v1/chat/completions", ChatCompletions),
            "https://polza.ai/api/v1/chat/completions"
        );
    }

    #[test]
    fn bearer_auth_only_when_key_is_set() {
        let p = Provider { base_url: "https://x.test/v1".into(), ..Default::default() };
        assert_eq!(upstream_headers(&p, "k1")[AUTHORIZATION], "Bearer k1");
        assert!(upstream_headers(&p, "  ").get(AUTHORIZATION).is_none());
        let or = Provider { base_url: "https://openrouter.ai/api/v1".into(), ..Default::default() };
        assert_eq!(upstream_headers(&or, "k")["x-title"], "Retranslator");
    }

    #[test]
    fn presets_fix_name_and_url() {
        let mut p = Provider { preset: "polza".into(), name: "x".into(), base_url: "https://evil.test".into(), ..Default::default() };
        apply_preset(&mut p);
        assert_eq!((p.name.as_str(), p.base_url.as_str()), ("PolzaAI", "https://polza.ai/api/v1"));
        // A preset that no longer exists keeps working as a custom provider.
        let mut old = Provider { preset: "deepseek".into(), name: "DeepSeek".into(), base_url: "https://api.deepseek.com".into(), ..Default::default() };
        apply_preset(&mut old);
        assert_eq!((old.preset.as_str(), old.base_url.as_str()), ("custom", "https://api.deepseek.com"));
        let mut c = Provider { preset: "".into(), name: "Мой".into(), base_url: "http://localhost:11434/v1".into(), ..Default::default() };
        apply_preset(&mut c);
        assert_eq!((c.preset.as_str(), c.name.as_str()), ("custom", "Мой"));
        assert!(fixed_preset("custom").is_none());
    }

    #[test]
    fn model_ids_from_various_shapes() {
        let openai = json!({"object":"list","data":[{"id":"gpt-b"},{"id":"gpt-a"},{"id":"gpt-a"}]});
        assert_eq!(parse_model_ids(&openai, false), vec!["gpt-a", "gpt-b"]);
        let gemini = json!({"data":[{"id":"models/gemini-2.5-flash"}]});
        assert_eq!(parse_model_ids(&gemini, true), vec!["gemini-2.5-flash"]);
        assert_eq!(parse_model_ids(&json!(["m1", {"name": "m2"}]), false), vec!["m1", "m2"]);
        assert!(parse_model_ids(&json!({"error": "x"}), false).is_empty());
    }

    #[test]
    fn quirks_by_host() {
        assert!(quirks("https://api.openai.com/v1").max_completion_tokens);
        assert!(!quirks("https://openrouter.ai/api/v1").max_completion_tokens);
        assert!(quirks("https://generativelanguage.googleapis.com/v1beta/openai").gemini_schema);
    }
}
