use crate::error::{FlowError, FlowErrorLocation, Result};
use greentic_types::i18n_text::I18nText;
use std::collections::{BTreeMap, BTreeSet};

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
    if let Some(locale) = explicit
        && !locale.trim().is_empty()
    {
        return locale.trim().to_string();
    }
    if let Ok(env_locale) = std::env::var("GREENTIC_LOCALE") {
        let trimmed = env_locale.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "en".to_string()
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
