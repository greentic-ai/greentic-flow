use crate::error::{FlowError, FlowErrorLocation, Result};
use greentic_types::i18n_text::I18nText;
use std::collections::{BTreeMap, BTreeSet};
use unic_langid::LanguageIdentifier;

#[derive(Debug, Clone, Default)]
pub struct I18nCatalog {
    entries: BTreeMap<String, BTreeMap<String, String>>,
}

impl I18nCatalog {
    pub fn insert(&mut self, key: impl Into<String>, locale: impl Into<String>, value: String) {
        self.entries
            .entry(key.into())
            .or_default()
            .insert(locale.into(), value);
    }

    pub fn get(&self, key: &str, locale: &str) -> Option<&str> {
        self.entries
            .get(key)
            .and_then(|locales| locales.get(locale))
            .map(|s| s.as_str())
    }
}

pub fn resolve_locale(explicit: Option<&str>) -> String {
    if let Some(locale) = normalize_locale(explicit.unwrap_or("")) {
        return locale;
    }
    if let Some(locale) = detect_env_locale() {
        return locale;
    }
    if let Some(raw) = sys_locale::get_locale()
        && let Some(locale) = normalize_locale(&raw)
    {
        return locale;
    }
    "en".to_string()
}

fn detect_env_locale() -> Option<String> {
    for key in ["GREENTIC_LOCALE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key)
            && let Some(locale) = normalize_locale(&value)
        {
            return Some(locale);
        }
    }
    None
}

fn normalize_locale(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("c")
        || trimmed.eq_ignore_ascii_case("posix")
    {
        return None;
    }
    let without_encoding = trimmed.split('.').next().unwrap_or(trimmed);
    let without_modifier = without_encoding
        .split('@')
        .next()
        .unwrap_or(without_encoding);
    let normalized = without_modifier.replace('_', "-");
    normalized
        .parse::<LanguageIdentifier>()
        .ok()
        .map(|lang| lang.to_string())
}

pub fn locale_fallback_chain(locale: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = locale.trim().to_string();
    if current.is_empty() {
        current = "en".to_string();
    }
    out.push(current.clone());
    if let Some((language, _region)) = current.split_once('-')
        && !language.is_empty()
        && language != "en"
    {
        out.push(language.to_string());
    }
    if !out.iter().any(|entry| entry == "en") {
        out.push("en".to_string());
    }
    out
}

pub fn resolve_text(text: &I18nText, catalog: &I18nCatalog, locale: &str) -> String {
    for candidate in locale_fallback_chain(locale) {
        if let Some(value) = catalog.get(text.key.as_str(), candidate.as_str()) {
            return value.to_string();
        }
    }
    text.fallback.clone().unwrap_or_else(|| text.key.clone())
}

pub fn resolve_cli_text(catalog: &I18nCatalog, locale: &str, key: &str, fallback: &str) -> String {
    let text = I18nText::new(key, Some(fallback.to_string()));
    resolve_text(&text, catalog, locale)
}

pub fn resolve_cli_template(
    catalog: &I18nCatalog,
    locale: &str,
    key: &str,
    fallback: &str,
    args: &[&str],
) -> String {
    let mut out = resolve_cli_text(catalog, locale, key, fallback);
    for arg in args {
        out = out.replacen("{}", arg, 1);
    }
    out
}

pub fn resolve_keys(
    keys: &BTreeSet<String>,
    catalog: &I18nCatalog,
    locale: &str,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for key in keys {
        let text = I18nText::new(key.as_str(), None);
        let value = resolve_text(&text, catalog, locale);
        if value.is_empty() {
            return Err(FlowError::Internal {
                message: format!("missing translation for key '{key}'"),
                location: FlowErrorLocation::new(None, None, None),
            });
        }
        out.insert(key.clone(), value);
    }
    Ok(out)
}
