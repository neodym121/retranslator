//! Interface language for messages the backend shows to the user (UI errors, tray, API errors).

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Ru,
    En,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == Lang::En as u8 {
        Lang::En
    } else {
        Lang::Ru
    }
}

pub fn pick<T>(lang: Lang, ru: T, en: T) -> T {
    match lang {
        Lang::Ru => ru,
        Lang::En => en,
    }
}

/// The text in the current language.
pub fn t<T>(ru: T, en: T) -> T {
    pick(current(), ru, en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_language() {
        assert_eq!(pick(Lang::Ru, "да", "yes"), "да");
        assert_eq!(pick(Lang::En, "да", "yes"), "yes");
        assert_eq!(serde_json::to_string(&Lang::En).unwrap(), "\"en\"");
    }
}
