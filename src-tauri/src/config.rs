//! Persistent settings (JSON in the app data dir) and secrets (system keyring).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};

use crate::i18n::Lang;
use crate::mapping::ModelRule;
use crate::providers::Provider;

pub const DEFAULT_PORT: u16 = 4870;
pub const CONFIG_FILE: &str = "config.json";
const KEYRING_SERVICE: &str = "Retranslator";
const LOCAL_KEY_ACCOUNT: &str = "local-api-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub port: u16,
    /// Start the gateway together with the app.
    pub autostart_server: bool,
    pub language: Lang,
    pub providers: Vec<Provider>,
    pub rules: Vec<ModelRule>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            autostart_server: true,
            language: Lang::Ru,
            providers: Vec::new(),
            rules: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE)
}

/// Loads the config. A missing file gives defaults; a broken one is kept aside
/// as `config.json.broken` so the user's data is not silently overwritten.
pub fn load(path: &Path) -> AppConfig {
    let Ok(text) = fs::read_to_string(path) else {
        return AppConfig::default();
    };
    match serde_json::from_str(&text) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("config: cannot parse {}: {err}", path.display());
            let _ = fs::copy(path, path.with_extension("json.broken"));
            AppConfig::default()
        }
    }
}

/// Writes the config atomically (temp file + rename).
pub fn save(path: &Path, cfg: &AppConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// A fresh local gateway key: `rtr-` + 48 alphanumeric characters.
pub fn generate_local_key() -> String {
    format!("rtr-{}", Alphanumeric.sample_string(&mut rand::rng(), 48))
}

pub fn provider_account(provider_id: &str) -> String {
    format!("provider:{provider_id}")
}

/// Where secrets live. The keyring in the app, memory in tests.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, String>;
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

pub struct KeyringStore;

impl KeyringStore {
    fn entry(account: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| format!("keyring: {e}"))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        match Self::entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keyring: {e}")),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        Self::entry(account)?
            .set_password(secret)
            .map_err(|e| format!("keyring: {e}"))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keyring: {e}")),
        }
    }
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>);

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(account.into(), secret.into());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

/// Reads the local gateway key, creating one on first launch. Never returns an empty key.
pub fn load_or_create_local_key(store: &dyn SecretStore) -> Result<String, String> {
    if let Some(key) = store.get(LOCAL_KEY_ACCOUNT)? {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }
    let key = generate_local_key();
    store.set(LOCAL_KEY_ACCOUNT, &key)?;
    Ok(key)
}

pub fn regenerate_local_key(store: &dyn SecretStore) -> Result<String, String> {
    let key = generate_local_key();
    store.set(LOCAL_KEY_ACCOUNT, &key)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_key_is_created_once_and_never_empty() {
        let store = MemoryStore::default();
        let first = load_or_create_local_key(&store).unwrap();
        assert!(first.starts_with("rtr-") && first.len() == 52);
        assert_eq!(load_or_create_local_key(&store).unwrap(), first);
        store.set(LOCAL_KEY_ACCOUNT, "  ").unwrap();
        let fresh = load_or_create_local_key(&store).unwrap();
        assert!(!fresh.trim().is_empty() && fresh != first);
    }

    #[test]
    fn config_roundtrip_and_broken_file_is_kept() {
        let dir = std::env::temp_dir().join(format!("rtr-cfg-{}", uuid::Uuid::new_v4()));
        let path = config_path(&dir);
        assert_eq!(load(&path).port, DEFAULT_PORT);
        let cfg = AppConfig { port: 5000, ..Default::default() };
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path).port, 5000);
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path).port, DEFAULT_PORT);
        assert!(path.with_extension("json.broken").exists());
        let _ = fs::remove_dir_all(dir);
    }
}
