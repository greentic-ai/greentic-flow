use assert_cmd::cargo::cargo_bin_cmd;
use greentic_flow::i18n::{I18nCatalog, locale_fallback_chain, resolve_locale, resolve_text};
use greentic_types::i18n_text::I18nText;
use predicates::str::contains;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("lock locale env mutex")
}

#[test]
fn locale_chain_falls_back_to_language_and_en() {
    let chain = locale_fallback_chain("nl-NL");
    assert_eq!(chain, vec!["nl-NL", "nl", "en"]);
}

#[test]
fn resolve_locale_prefers_explicit_then_env_then_system_then_en() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("GREENTIC_LOCALE");
        std::env::remove_var("LC_ALL");
        std::env::remove_var("LC_MESSAGES");
        std::env::remove_var("LANG");
    }
    assert_eq!(resolve_locale(Some("pt-BR")), "pt-BR");
    unsafe {
        std::env::set_var("LC_ALL", "fr_FR.UTF-8");
    }
    assert_eq!(resolve_locale(None), "fr-FR");
    unsafe {
        std::env::remove_var("LC_ALL");
        std::env::set_var("LANG", "nl_NL.UTF-8");
    }
    assert_eq!(resolve_locale(None), "nl-NL");
    unsafe {
        std::env::remove_var("LANG");
    }
    assert_eq!(resolve_locale(None), "en");
}

#[test]
fn resolve_locale_prefers_lc_all_over_other_system_vars() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("GREENTIC_LOCALE");
        std::env::set_var("LC_ALL", "de_DE.UTF-8");
        std::env::set_var("LC_MESSAGES", "es_ES.UTF-8");
        std::env::set_var("LANG", "nl_NL.UTF-8");
    }
    assert_eq!(resolve_locale(None), "de-DE");
    unsafe {
        std::env::remove_var("LC_ALL");
        std::env::remove_var("LC_MESSAGES");
        std::env::remove_var("LANG");
    }
}

#[test]
fn resolve_locale_keeps_greentic_override_for_compat() {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("GREENTIC_LOCALE", "es_MX.UTF-8");
        std::env::set_var("LC_ALL", "de_DE.UTF-8");
    }
    assert_eq!(resolve_locale(None), "es-MX");
    unsafe {
        std::env::remove_var("GREENTIC_LOCALE");
        std::env::remove_var("LC_ALL");
    }
}

#[test]
fn resolve_text_prefers_catalog_then_fallback_then_key() {
    let mut catalog = I18nCatalog::default();
    catalog.insert("greeting", "nl", "Hallo".to_string());
    catalog.insert("greeting", "en", "Hello".to_string());

    let text = I18nText::new("greeting", Some("Hi".to_string()));
    assert_eq!(resolve_text(&text, &catalog, "nl-NL"), "Hallo");

    let text = I18nText::new("missing", Some("Fallback".to_string()));
    assert_eq!(resolve_text(&text, &catalog, "nl-NL"), "Fallback");

    let text = I18nText::new("missing2", None);
    assert_eq!(resolve_text(&text, &catalog, "nl-NL"), "missing2");
}

#[test]
fn cli_help_uses_requested_non_english_locale() {
    cargo_bin_cmd!("greentic-flow")
        .arg("--locale")
        .arg("es")
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Ayudantes de andamiaje de flujos"));
}
