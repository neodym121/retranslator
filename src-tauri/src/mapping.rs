//! Model mapping rules: a Claude-looking local id → provider + real model id.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::AppConfig;
use crate::i18n::t;
use crate::providers::Provider;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Sonnet,
    Opus,
    Haiku,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelRule {
    pub id: String,
    /// The id Claude Desktop sees and sends, e.g. `claude-sonnet-5.5`.
    pub local_id: String,
    pub provider_id: String,
    /// The real model id at the provider, e.g. `deepseek-v4.1-flash`.
    pub upstream_model: String,
    /// The aggregator's sub-provider to pin the request to, e.g. `DeepInfra`. Empty: the aggregator
    /// picks. Only used when the rule's provider is an aggregator.
    pub sub_provider: String,
    pub enabled: bool,
    pub display_name: String,
    pub tier: Option<Tier>,
    /// `is_family_default` for the tier in `/v1/models`.
    pub family_default: bool,
    pub max_input_tokens: Option<u64>,
    pub supports_1m: bool,
    /// Caps `max_tokens` sent upstream (many models allow far less output than Claude asks for).
    pub max_output_tokens: Option<u64>,
    pub created_at: String,
}

// The same check Claude Desktop applies to gateway model ids (from its bundled source):
// a family word must be present and none of the other vendors' names may appear.
static TIER_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(sonnet|opus|haiku|fable|mythos)(-[\d.]+)?$").unwrap());
static FOREIGN_VENDOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"ark-code|astron|command-r|deepseek|doubao|gemini|gemma|glm|gpt|grok|hermes|hy3|kimi|lfm|\bling\b|",
        r"llama|longcat|mimo|minimax|mistral|mixtral|moonshot|nemotron|openai|phi-|qianfan|qwen|tc-code|",
        r"\bunic\b|yi-|stepfun|step-3|seed-|bytedance|hunyuan|granite|amazon\.nova|nova-|devstral|ministral|",
        r"ernie|codex|arcee|trinity|abab|phi\d|\bk2\.|\bm2\.|jamba|arctic|solar|mercury|zamba|kat-coder|\bds-|dpsk"
    ))
    .unwrap()
});
const FAMILY_WORDS: &[&str] = &["claude", "sonnet", "opus", "haiku", "fable", "mythos", "anthropic"];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IdCheck {
    /// Hard error: the rule cannot be saved.
    pub error: Option<String>,
    /// Claude Desktop will hide or reject this id; saving is still allowed.
    pub warning: Option<String>,
}

pub fn check_local_id(id: &str) -> IdCheck {
    let id = id.trim();
    let mut check = IdCheck { error: None, warning: None };
    if id.is_empty() {
        check.error = Some(t("Локальный id не может быть пустым", "Local id cannot be empty").into());
        return check;
    }
    if id.len() > 255 || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        check.error = Some(
            t("Id без пробелов и управляющих символов, до 255 знаков", "Id without spaces or control characters, up to 255 chars")
                .into(),
        );
        return check;
    }
    if strip_1m_suffix(id).len() != id.len() {
        check.error = Some(
            t(
                "Суффикс [1m] Claude Desktop добавляет сам: включите «supports_1m» в правиле",
                "Claude Desktop adds the [1m] suffix itself: turn on supports_1m in the rule",
            )
            .into(),
        );
        return check;
    }
    let lower = id.to_ascii_lowercase();
    if let Some(m) = FOREIGN_VENDOR.find(&lower) {
        check.warning = Some(t(
            format!("Claude Desktop отбросит этот id: в нём есть «{}», а клиент не принимает имена чужих моделей", m.as_str()),
            format!("Claude Desktop will drop this id: it contains \"{}\", and the client rejects other vendors' model names", m.as_str()),
        ));
    } else if !TIER_ONLY.is_match(&lower) && !FAMILY_WORDS.iter().any(|w| lower.contains(w)) {
        check.warning = Some(
            t(
                "Claude Desktop покажет только id с claude, sonnet, opus, haiku или anthropic (например claude-sonnet-5.5)",
                "Claude Desktop only shows ids with claude, sonnet, opus, haiku or anthropic (e.g. claude-sonnet-5.5)",
            )
            .into(),
        );
    }
    check
}

/// Removes the `[1m]` suffix Claude Desktop appends for the 1M-context variant.
pub fn strip_1m_suffix(model: &str) -> &str {
    let t = model.trim();
    match t.len().checked_sub(4).and_then(|i| Some((i, t.get(i..)?))) {
        Some((i, tail)) if tail.eq_ignore_ascii_case("[1m]") => t[..i].trim_end(),
        _ => t,
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    pub rule: ModelRule,
    pub provider: Provider,
}

impl Route {
    /// The sub-provider to pin to: set on the rule, and the provider must be an aggregator.
    pub fn sub_provider(&self) -> Option<&str> {
        let name = self.rule.sub_provider.trim();
        (self.provider.aggregator && !name.is_empty()).then_some(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    UnknownModel(String),
    MissingProvider(String),
}

impl RouteError {
    pub fn message(&self) -> String {
        match self {
            RouteError::UnknownModel(m) => t(
                format!("model: {m} — в Retranslator нет включённого правила для этой модели. Добавьте его во вкладке «Модели»."),
                format!("model: {m} — Retranslator has no enabled rule for this model. Add one on the Models tab."),
            ),
            RouteError::MissingProvider(m) => t(
                format!("model: {m} — провайдер этого правила удалён. Выберите другой во вкладке «Модели»."),
                format!("model: {m} — the provider of this rule was deleted. Pick another one on the Models tab."),
            ),
        }
    }
}

/// Finds the enabled rule for a model id from the client (case-insensitive, `[1m]` ignored).
pub fn resolve(cfg: &AppConfig, model: &str) -> Result<Route, RouteError> {
    let wanted = strip_1m_suffix(model);
    let rule = cfg
        .rules
        .iter()
        .find(|r| r.enabled && r.local_id.trim().eq_ignore_ascii_case(wanted))
        .ok_or_else(|| RouteError::UnknownModel(model.trim().to_string()))?;
    let provider = cfg
        .provider(&rule.provider_id)
        .ok_or_else(|| RouteError::MissingProvider(model.trim().to_string()))?;
    Ok(Route { rule: rule.clone(), provider: provider.clone() })
}

/// Validates a rule before saving it into `cfg`.
pub fn validate_rule(cfg: &AppConfig, rule: &ModelRule) -> Result<IdCheck, String> {
    let check = check_local_id(&rule.local_id);
    if let Some(err) = &check.error {
        return Err(err.clone());
    }
    if rule.upstream_model.trim().is_empty() {
        return Err(t("Укажите реальный id модели у провайдера", "Enter the provider's real model id").into());
    }
    if cfg.provider(&rule.provider_id).is_none() {
        return Err(t("Выберите провайдера", "Pick a provider").into());
    }
    if cfg
        .rules
        .iter()
        .any(|r| r.id != rule.id && r.local_id.trim().eq_ignore_ascii_case(rule.local_id.trim()))
    {
        let id = rule.local_id.trim();
        return Err(t(format!("Правило для «{id}» уже есть"), format!("A rule for \"{id}\" already exists")));
    }
    if rule.max_input_tokens == Some(0) || rule.max_output_tokens == Some(0) {
        return Err(t("Лимиты токенов должны быть больше нуля", "Token limits must be greater than zero").into());
    }
    Ok(check)
}

/// The `/v1/models` body in Anthropic format.
pub fn models_response(cfg: &AppConfig) -> Value {
    let data: Vec<Value> = cfg
        .rules
        .iter()
        .filter(|r| r.enabled && cfg.provider(&r.provider_id).is_some())
        .map(model_entry)
        .collect();
    let first = data.first().and_then(|m| m.get("id")).cloned().unwrap_or(Value::Null);
    let last = data.last().and_then(|m| m.get("id")).cloned().unwrap_or(Value::Null);
    // `object` makes the same list valid for OpenAI-style clients.
    json!({ "object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last })
}

fn model_entry(rule: &ModelRule) -> Value {
    let id = rule.local_id.trim();
    let mut m = Map::new();
    m.insert("type".into(), json!("model"));
    m.insert("id".into(), json!(id));
    let display = rule.display_name.trim();
    m.insert("display_name".into(), json!(if display.is_empty() { id } else { display }));
    let created = if rule.created_at.is_empty() { "2025-01-01T00:00:00Z" } else { &rule.created_at };
    m.insert("created_at".into(), json!(created));
    // OpenAI's model object, alongside Anthropic's fields.
    m.insert("object".into(), json!("model"));
    m.insert("owned_by".into(), json!("retranslator"));
    let unix = chrono::DateTime::parse_from_rfc3339(created).map(|d| d.timestamp()).unwrap_or(0);
    m.insert("created".into(), json!(unix));
    if let Some(n) = rule.max_input_tokens {
        m.insert("max_input_tokens".into(), json!(n));
    }
    m.insert("supports_1m".into(), json!(rule.supports_1m));
    if let Some(tier) = rule.tier {
        m.insert("anthropic_family_tier".into(), serde_json::to_value(tier).unwrap());
        if rule.family_default {
            m.insert("is_family_default".into(), json!(true));
        }
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AppConfig {
        AppConfig {
            providers: vec![Provider { id: "p1".into(), name: "DS".into(), ..Default::default() }],
            rules: vec![
                ModelRule {
                    id: "r1".into(),
                    local_id: "claude-sonnet-5.5".into(),
                    provider_id: "p1".into(),
                    upstream_model: "deepseek-v4.1-flash".into(),
                    enabled: true,
                    tier: Some(Tier::Sonnet),
                    family_default: true,
                    max_input_tokens: Some(1_000_000),
                    supports_1m: true,
                    ..Default::default()
                },
                ModelRule {
                    id: "r2".into(),
                    local_id: "claude-opus-5.5".into(),
                    provider_id: "p1".into(),
                    upstream_model: "x".into(),
                    enabled: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn local_id_check_mirrors_claude_desktop() {
        assert_eq!(check_local_id("claude-sonnet-5.5"), IdCheck { error: None, warning: None });
        assert!(check_local_id("sonnet").warning.is_none());
        assert!(check_local_id("anthropic/claude-x").warning.is_none());
        assert!(check_local_id("my-model").warning.is_some());
        let w = check_local_id("claude-deepseek").warning.unwrap();
        assert!(w.contains("deepseek"));
        assert!(check_local_id("claude-gpt-5").warning.is_some());
        assert!(check_local_id("").error.is_some());
        assert!(check_local_id("claude sonnet").error.is_some());
        assert!(check_local_id("claude-sonnet-5.5[1m]").error.is_some());
    }

    #[test]
    fn resolve_strips_1m_and_ignores_case_and_disabled_rules() {
        let c = cfg();
        let r = resolve(&c, "claude-sonnet-5.5[1m]").unwrap();
        assert_eq!(r.rule.upstream_model, "deepseek-v4.1-flash");
        assert!(resolve(&c, "Claude-Sonnet-5.5").is_ok());
        assert_eq!(
            resolve(&c, "claude-opus-5.5").unwrap_err(),
            RouteError::UnknownModel("claude-opus-5.5".into())
        );
        let mut c2 = c.clone();
        c2.providers.clear();
        assert!(matches!(resolve(&c2, "claude-sonnet-5.5"), Err(RouteError::MissingProvider(_))));
    }

    #[test]
    fn sub_provider_needs_an_aggregator_and_a_name() {
        let mut c = cfg();
        c.rules[0].sub_provider = "  DeepInfra ".into();
        assert_eq!(resolve(&c, "claude-sonnet-5.5").unwrap().sub_provider(), None);
        c.providers[0].aggregator = true;
        assert_eq!(resolve(&c, "claude-sonnet-5.5").unwrap().sub_provider(), Some("DeepInfra"));
        c.rules[0].sub_provider = "  ".into();
        assert_eq!(resolve(&c, "claude-sonnet-5.5").unwrap().sub_provider(), None);
    }

    #[test]
    fn models_list_in_anthropic_format() {
        let body = models_response(&cfg());
        assert_eq!(body["has_more"], false);
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        let m = &data[0];
        assert_eq!(m["id"], "claude-sonnet-5.5");
        assert_eq!(m["type"], "model");
        assert_eq!(m["display_name"], "claude-sonnet-5.5");
        assert_eq!(m["anthropic_family_tier"], "sonnet");
        assert_eq!(m["is_family_default"], true);
        assert_eq!(m["supports_1m"], true);
        assert_eq!(m["max_input_tokens"], 1_000_000);
        assert!(m["created_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!((body["object"].as_str(), m["object"].as_str(), m["owned_by"].as_str()), (Some("list"), Some("model"), Some("retranslator")));
        assert!(m["created"].as_i64().unwrap() > 1_700_000_000);
        assert_eq!(body["first_id"], "claude-sonnet-5.5");
    }

    #[test]
    fn rule_validation() {
        let c = cfg();
        let mut rule = ModelRule {
            id: "new".into(),
            local_id: "CLAUDE-SONNET-5.5".into(),
            provider_id: "p1".into(),
            upstream_model: "m".into(),
            ..Default::default()
        };
        assert!(validate_rule(&c, &rule).unwrap_err().contains("уже есть"));
        rule.local_id = "claude-haiku-5".into();
        assert!(validate_rule(&c, &rule).is_ok());
        rule.provider_id = "nope".into();
        assert!(validate_rule(&c, &rule).is_err());
    }
}
