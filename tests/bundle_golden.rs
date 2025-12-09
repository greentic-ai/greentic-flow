use greentic_flow::{
    canonicalize_json, compile_flow, extract_component_pins, load_and_validate_bundle,
    loader::load_ygtc_from_str,
};
use std::collections::HashMap;

#[test]
fn bundle_fields_are_stable() {
    let yaml = std::fs::read_to_string("fixtures/weather_bot.ygtc").unwrap();
    let bundle = load_and_validate_bundle(&yaml, None).unwrap();
    assert_eq!(bundle.id, "weather_bot");
    assert_eq!(bundle.kind, "messaging");
    assert_eq!(bundle.entry, "in");
    assert_eq!(bundle.json, canonicalize_json(&bundle.json));

    let bundle_again = load_and_validate_bundle(&yaml, None).unwrap();
    assert_eq!(bundle.hash_blake3, bundle_again.hash_blake3);

    let mut names: HashMap<_, _> = bundle
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node.component.name.as_str()))
        .collect();
    assert_eq!(names.remove("in"), Some("qa.process"));
    assert_eq!(names.remove("forecast_weather"), Some("mcp.exec"));
    assert_eq!(names.remove("weather_text"), Some("templating.handlebars"));
    assert!(names.is_empty());
}

#[test]
fn extract_component_pins_matches_bundle_nodes() {
    let yaml = std::fs::read_to_string("fixtures/weather_bot.ygtc").unwrap();
    let bundle = load_and_validate_bundle(&yaml, None).unwrap();

    let doc = load_ygtc_from_str(&yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let pins = extract_component_pins(&flow);

    let map: HashMap<_, _> = pins
        .into_iter()
        .map(|(node_id, pin)| (node_id, pin.name))
        .collect();

    for node in bundle.nodes {
        assert_eq!(
            map.get(&node.node_id),
            Some(&node.component.name),
            "component pin mismatch for {}",
            node.node_id
        );
    }
}
