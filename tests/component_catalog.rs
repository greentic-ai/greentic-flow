use greentic_flow::component_catalog::{ComponentCatalog, ManifestCatalog};
use serde_json::json;
use tempfile::NamedTempFile;

#[test]
fn catalog_resolves_component_exec_alias() {
    let manifest = json!({
        "id": "ai.greentic.hello",
        "config_schema": { "required": ["message"] },
        "dev_flows": {}
    });
    let file = NamedTempFile::new().expect("temp file");
    std::fs::write(file.path(), manifest.to_string()).expect("write manifest");

    let catalog = ManifestCatalog::load_from_paths(&[file.path()]);
    let exec = catalog
        .resolve("component.exec")
        .expect("component.exec present");
    assert!(exec.required_fields.is_empty());
    let component = catalog
        .resolve("ai.greentic.hello")
        .expect("component present");
    assert_eq!(component.required_fields, vec!["message".to_string()]);
}
