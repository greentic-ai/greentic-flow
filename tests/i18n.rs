use greentic_flow::i18n::{I18nCatalog, locale_fallback_chain, resolve_locale, resolve_text};
use greentic_types::i18n_text::I18nText;

#[test]
fn locale_chain_falls_back_to_language_and_en() {
    let chain = locale_fallback_chain("nl-NL");
    assert_eq!(chain, vec!["nl-NL", "nl", "en"]);
}

#[test]
fn resolve_locale_prefers_explicit_then_env_then_en() {
    unsafe {
        std::env::remove_var("GREENTIC_LOCALE");
    }
    assert_eq!(resolve_locale(Some("pt-BR")), "pt-BR");
    unsafe {
        std::env::set_var("GREENTIC_LOCALE", "fr");
    }
    assert_eq!(resolve_locale(None), "fr");
    unsafe {
        std::env::remove_var("GREENTIC_LOCALE");
    }
    assert_eq!(resolve_locale(None), "en");
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
