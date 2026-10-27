//! Shared core used by the HTTP server and the UI commands: config, secrets, logs, HTTP client.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use crate::config::{self, AppConfig, SecretStore};
use crate::logs::{LogEntry, Logs};

pub enum Notice {
    Log(Box<LogEntry>),
    State,
}

type Notifier = Arc<dyn Fn(Notice) + Send + Sync>;

pub struct Core {
    config_path: PathBuf,
    config: RwLock<AppConfig>,
    secrets: Box<dyn SecretStore>,
    provider_keys: RwLock<HashMap<String, String>>,
    local_key: RwLock<String>,
    pub logs: Logs,
    pub http: reqwest::Client,
    notifier: RwLock<Option<Notifier>>,
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("Retranslator/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        // Generous: reasoning models can stay silent for minutes. Clients get pings meanwhile.
        .read_timeout(Duration::from_secs(900))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("http client")
}

impl Core {
    /// Loads config and secrets. Provider keys are cached so requests never touch the keyring.
    pub fn load(config_path: PathBuf, secrets: Box<dyn SecretStore>) -> Result<Self, String> {
        let mut cfg = config::load(&config_path);
        // Preset providers follow the preset (also fixes configs from older builds).
        cfg.providers.iter_mut().for_each(crate::providers::apply_preset);
        crate::i18n::set(cfg.language);
        if !config_path.exists() {
            // First launch: write the defaults so the config file exists.
            if let Err(e) = config::save(&config_path, &cfg) {
                eprintln!("config: {e}");
            }
        }
        let local_key = config::load_or_create_local_key(secrets.as_ref())?;
        let mut keys = HashMap::new();
        for p in &cfg.providers {
            match secrets.get(&config::provider_account(&p.id)) {
                Ok(Some(k)) => {
                    keys.insert(p.id.clone(), k);
                }
                Ok(None) => {}
                Err(e) => eprintln!("keyring: cannot read key of {}: {e}", p.name),
            }
        }
        Ok(Self {
            config_path,
            config: RwLock::new(cfg),
            secrets,
            provider_keys: RwLock::new(keys),
            local_key: RwLock::new(local_key),
            logs: Logs::default(),
            http: http_client(),
            notifier: RwLock::new(None),
        })
    }

    pub fn config(&self) -> RwLockReadGuard<'_, AppConfig> {
        self.config.read().unwrap()
    }

    /// Applies a change to a copy, saves it, then swaps it in; a failed save changes nothing.
    pub fn update_config<R>(&self, change: impl FnOnce(&mut AppConfig) -> Result<R, String>) -> Result<R, String> {
        let mut guard = self.config.write().unwrap();
        let mut next = guard.clone();
        let result = change(&mut next)?;
        config::save(&self.config_path, &next)?;
        *guard = next;
        drop(guard);
        self.notify(Notice::State);
        Ok(result)
    }

    pub fn local_key(&self) -> String {
        self.local_key.read().unwrap().clone()
    }

    pub fn regenerate_local_key(&self) -> Result<String, String> {
        let key = config::regenerate_local_key(self.secrets.as_ref())?;
        *self.local_key.write().unwrap() = key.clone();
        self.notify(Notice::State);
        Ok(key)
    }

    pub fn provider_key(&self, provider_id: &str) -> String {
        self.provider_keys.read().unwrap().get(provider_id).cloned().unwrap_or_default()
    }

    /// `None` removes the stored key.
    pub fn set_provider_key(&self, provider_id: &str, key: Option<&str>) -> Result<(), String> {
        let account = config::provider_account(provider_id);
        match key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => {
                self.secrets.set(&account, k)?;
                self.provider_keys.write().unwrap().insert(provider_id.into(), k.into());
            }
            None => {
                self.secrets.delete(&account)?;
                self.provider_keys.write().unwrap().remove(provider_id);
            }
        }
        Ok(())
    }

    /// `sk-…a1b2` style hint for the UI; the key itself never leaves the backend.
    pub fn key_hint(&self, provider_id: &str) -> Option<String> {
        let keys = self.provider_keys.read().unwrap();
        let key = keys.get(provider_id)?;
        let chars: Vec<char> = key.chars().collect();
        if chars.len() <= 8 {
            return Some("••••".into());
        }
        let head: String = chars[..3].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        Some(format!("{head}…{tail}"))
    }

    pub fn set_notifier(&self, notifier: Notifier) {
        *self.notifier.write().unwrap() = Some(notifier);
    }

    pub fn notify(&self, notice: Notice) {
        let notifier = self.notifier.read().unwrap().clone();
        if let Some(n) = notifier {
            n(notice);
        }
    }

    pub fn record(&self, entry: LogEntry) {
        let entry = self.logs.push(entry);
        self.notify(Notice::Log(Box::new(entry)));
    }
}

#[cfg(test)]
pub fn test_core(cfg: AppConfig) -> Arc<Core> {
    let dir = std::env::temp_dir().join(format!("rtr-test-{}", uuid::Uuid::new_v4()));
    let path = config::config_path(&dir);
    config::save(&path, &cfg).unwrap();
    Arc::new(Core::load(path, Box::new(config::MemoryStore::default())).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hints_and_config_updates() {
        let core = test_core(AppConfig::default());
        assert!(core.local_key().starts_with("rtr-"));
        let old = core.local_key();
        assert_ne!(core.regenerate_local_key().unwrap(), old);

        core.set_provider_key("p", Some("sk-abcdefghijkl1234")).unwrap();
        assert_eq!(core.provider_key("p"), "sk-abcdefghijkl1234");
        assert_eq!(core.key_hint("p").unwrap(), "sk-…1234");
        core.set_provider_key("p", None).unwrap();
        assert_eq!(core.provider_key("p"), "");

        let r: Result<(), String> = core.update_config(|c| {
            c.port = 1;
            Err("nope".into())
        });
        assert!(r.is_err());
        assert_eq!(core.config().port, config::DEFAULT_PORT, "failed change is not applied");
        core.update_config(|c| {
            c.port = 5555;
            Ok(())
        })
        .unwrap();
        assert_eq!(config::load(&core.config_path).port, 5555);
    }
}
