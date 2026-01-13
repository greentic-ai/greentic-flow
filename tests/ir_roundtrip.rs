use greentic_flow::{compile_flow, loader::load_ygtc_from_str};

#[test]
fn ir_serializes() {
    let yaml = std::fs::read_to_string("fixtures/weather_bot.ygtc").unwrap();
    let doc = load_ygtc_from_str(&yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let json = serde_json::to_string(&flow).unwrap();
    assert!(json.contains("\"id\":\"component.exec\""));
    assert!(json.contains("\"operation\":\"qa.process\""));
}
