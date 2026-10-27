//! Tauri commands for the UI. Every mutating command returns the fresh state, so the UI never
//! keeps its own copy of anything.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt;

use crate::i18n::{self, t, Lang};
use crate::logs::LogEntry;
use crate::mapping::{self, IdCheck, ModelRule};
use crate::providers::{self, ConnectionCheck, ModelList, Preset, Provider, PRESETS};
use crate::server::{Server, ServerStatus};
use crate::state::Core;

pub struct AppState {
    pub core: Arc<Core>,
    pub server: Arc<Server>,
    pub app: AppHandle,
}

#[derive(Serialize)]
pub struct ProviderView {
    #[serde(flatten)]
    pub provider: Provider,
    pub key_hint: Option<String>,
}

#[derive(Serialize)]
pub struct RuleView {
    #[serde(flatten)]
    pub rule: ModelRule,
    pub warning: Option<String>,
}

#[derive(Serialize)]
pub struct UiState {
    pub server: ServerStatus,
    pub base_url: String,
    pub local_key: String,
    pub port: u16,
    pub autostart_server: bool,
    /// Launch at Windows login. Read from the system, so a change made there shows up too.
    pub autostart_app: bool,
    pub language: Lang,
    pub providers: Vec<ProviderView>,
    pub rules: Vec<RuleView>,
    pub presets: &'static [Preset],
    pub requests_total: u64,
}

pub fn snapshot(state: &AppState) -> UiState {
    let core = &state.core;
    let cfg = core.config();
    UiState {
        server: state.server.status(),
        base_url: format!("http://127.0.0.1:{}", cfg.port),
        local_key: core.local_key(),
        port: cfg.port,
        autostart_server: cfg.autostart_server,
        autostart_app: state.app.autolaunch().is_enabled().unwrap_or(false),
        language: cfg.language,
        providers: cfg
            .providers
            .iter()
            .map(|p| ProviderView { provider: p.clone(), key_hint: core.key_hint(&p.id) })
            .collect(),
        rules: cfg
            .rules
            .iter()
            .map(|r| RuleView { rule: r.clone(), warning: mapping::check_local_id(&r.local_id).warning })
            .collect(),
        presets: PRESETS,
        requests_total: core.logs.total(),
    }
}

#[tauri::command]
pub fn get_state(state: State<'_, AppState>) -> UiState {
    snapshot(&state)
}

#[tauri::command]
pub async fn start_server(state: State<'_, AppState>) -> Result<UiState, String> {
    state.server.start(state.core.clone()).await?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub async fn stop_server(state: State<'_, AppState>) -> Result<UiState, String> {
    state.server.stop(&state.core).await;
    Ok(snapshot(&state))
}

#[tauri::command]
pub async fn set_port(state: State<'_, AppState>, port: u16) -> Result<UiState, String> {
    if port < 1024 {
        return Err(t("Порт должен быть от 1024 до 65535", "The port must be between 1024 and 65535").into());
    }
    let changed = state.core.config().port != port;
    state.core.update_config(|c| {
        c.port = port;
        Ok(())
    })?;
    if changed && state.server.status().running {
        state.server.restart(state.core.clone()).await?;
    }
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn set_autostart_server(state: State<'_, AppState>, enabled: bool) -> Result<UiState, String> {
    state.core.update_config(|c| {
        c.autostart_server = enabled;
        Ok(())
    })?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn set_autostart_app(state: State<'_, AppState>, enabled: bool) -> Result<UiState, String> {
    let launch = state.app.autolaunch();
    let result = if enabled { launch.enable() } else { launch.disable() };
    result.map_err(|e| t(format!("Не удалось изменить автозапуск: {e}"), format!("Could not change autostart: {e}")))?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn set_language(state: State<'_, AppState>, language: Lang) -> Result<UiState, String> {
    i18n::set(language);
    state.core.update_config(|c| {
        c.language = language;
        Ok(())
    })?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn regenerate_key(state: State<'_, AppState>) -> Result<UiState, String> {
    state.core.regenerate_local_key()?;
    Ok(snapshot(&state))
}

#[derive(Debug, Deserialize)]
pub struct ProviderInput {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub preset: String,
    /// Ignored for presets: they keep their own name and URL.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub aggregator: bool,
    /// Empty keeps the stored key.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub clear_key: bool,
}

fn check_base_url(url: &str) -> Result<(), String> {
    match reqwest::Url::parse(url.trim()) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(()),
        _ => Err(t(
            "Base URL должен начинаться с https:// (или http:// для локальных серверов)",
            "Base URL must start with https:// (or http:// for local servers)",
        )
        .into()),
    }
}

#[tauri::command]
pub fn save_provider(state: State<'_, AppState>, provider: ProviderInput) -> Result<UiState, String> {
    let id = if provider.id.is_empty() { uuid::Uuid::new_v4().to_string() } else { provider.id.clone() };
    let mut value = Provider {
        id: id.clone(),
        name: provider.name.trim().to_string(),
        preset: provider.preset.clone(),
        base_url: provider.base_url.trim().trim_end_matches('/').to_string(),
        aggregator: provider.aggregator,
    };
    providers::apply_preset(&mut value);
    check_base_url(&value.base_url)?;
    if value.name.is_empty() {
        return Err(t("Укажите название провайдера", "Enter a provider name").into());
    }
    // Key first: if the keyring refuses, the provider is not saved half-way.
    if provider.clear_key {
        state.core.set_provider_key(&id, None)?;
    } else if !provider.api_key.trim().is_empty() {
        state.core.set_provider_key(&id, Some(&provider.api_key))?;
    }
    state.core.update_config(|c| {
        match c.providers.iter_mut().find(|p| p.id == id) {
            Some(existing) => *existing = value,
            None => c.providers.push(value),
        }
        Ok(())
    })?;
    Ok(snapshot(&state))
}

/// The stored key of a provider, for the edit form (the key never rides along with the shared state).
#[tauri::command]
pub fn get_provider_key(state: State<'_, AppState>, id: String) -> String {
    state.core.provider_key(&id)
}

#[tauri::command]
pub fn delete_provider(state: State<'_, AppState>, id: String) -> Result<UiState, String> {
    state.core.update_config(|c| {
        c.providers.retain(|p| p.id != id);
        c.rules.retain(|r| r.provider_id != id);
        Ok(())
    })?;
    state.core.set_provider_key(&id, None)?;
    Ok(snapshot(&state))
}

/// A provider as typed in the form, possibly not saved yet.
#[derive(Debug, Deserialize)]
pub struct ProviderDraft {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub preset: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

impl ProviderDraft {
    fn resolve(&self, core: &Core) -> Result<(Provider, String), String> {
        let key = if self.api_key.trim().is_empty() { core.provider_key(&self.id) } else { self.api_key.trim().to_string() };
        let mut provider = Provider {
            id: self.id.clone(),
            preset: self.preset.clone(),
            base_url: self.base_url.clone(),
            ..Default::default()
        };
        providers::apply_preset(&mut provider);
        check_base_url(&provider.base_url)?;
        Ok((provider, key))
    }
}

#[tauri::command]
pub async fn fetch_models(state: State<'_, AppState>, draft: ProviderDraft) -> Result<ModelList, String> {
    let (provider, key) = draft.resolve(&state.core)?;
    providers::fetch_models(&state.core.http, &provider, &key).await
}

#[tauri::command]
pub async fn test_provider(state: State<'_, AppState>, draft: ProviderDraft) -> Result<ConnectionCheck, String> {
    let (provider, key) = draft.resolve(&state.core)?;
    Ok(providers::check_connection(&state.core.http, &provider, &key).await)
}

#[tauri::command]
pub fn save_rule(state: State<'_, AppState>, rule: ModelRule) -> Result<UiState, String> {
    state.core.update_config(|c| {
        let mut rule = rule;
        rule.local_id = rule.local_id.trim().to_string();
        rule.upstream_model = rule.upstream_model.trim().to_string();
        rule.display_name = rule.display_name.trim().to_string();
        rule.sub_provider = rule.sub_provider.trim().to_string();
        if !c.provider(&rule.provider_id).is_some_and(|p| p.aggregator) {
            rule.sub_provider.clear();
        }
        if rule.tier.is_none() {
            rule.family_default = false;
        }
        mapping::validate_rule(c, &rule)?;
        // One default per tier.
        if rule.family_default {
            for other in c.rules.iter_mut().filter(|r| r.id != rule.id && r.tier == rule.tier) {
                other.family_default = false;
            }
        }
        match c.rules.iter_mut().find(|r| !rule.id.is_empty() && r.id == rule.id) {
            Some(existing) => {
                rule.created_at = existing.created_at.clone();
                *existing = rule;
            }
            None => {
                rule.id = uuid::Uuid::new_v4().to_string();
                rule.created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                c.rules.push(rule);
            }
        }
        Ok(())
    })?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn delete_rule(state: State<'_, AppState>, id: String) -> Result<UiState, String> {
    state.core.update_config(|c| {
        c.rules.retain(|r| r.id != id);
        Ok(())
    })?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn set_rule_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> Result<UiState, String> {
    state.core.update_config(|c| {
        let local_id = {
            let rule = c.rules.iter_mut().find(|r| r.id == id).ok_or_else(|| t("Правило не найдено", "Rule not found").to_string())?;
            rule.enabled = enabled;
            rule.local_id.clone()
        };
        if enabled
            && c.rules.iter().filter(|r| r.enabled && r.local_id.eq_ignore_ascii_case(&local_id)).count() > 1
        {
            return Err(t(
                format!("Уже есть включённое правило для «{local_id}»"),
                format!("There is already an enabled rule for \"{local_id}\""),
            ));
        }
        Ok(())
    })?;
    Ok(snapshot(&state))
}

#[tauri::command]
pub fn check_local_id(id: String) -> IdCheck {
    mapping::check_local_id(&id)
}

#[tauri::command]
pub fn get_logs(state: State<'_, AppState>) -> Vec<LogEntry> {
    state.core.logs.list()
}

#[tauri::command]
pub fn clear_logs(state: State<'_, AppState>) {
    state.core.logs.clear();
}
