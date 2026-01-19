use handlebars::{
    Context, Handlebars, Helper, HelperDef, Output, RenderContext, RenderError, RenderErrorReason,
    Renderable,
};
use regex::Regex;
use serde_json::{Map, Value};

use crate::error::{FlowError, FlowErrorLocation, Result};

const STATE_TOKEN_PREFIX: &str = "__STATE_TOKEN__";
const STATE_TOKEN_SUFFIX: &str = "__";

pub struct TemplateRenderer {
    handlebars: Handlebars<'static>,
    manifest_id: Option<String>,
}

impl TemplateRenderer {
    pub fn new(manifest_id: Option<String>) -> Self {
        let mut handlebars = Handlebars::new();
        handlebars.register_escape_fn(|s| s.to_string());
        handlebars.register_helper("json", Box::new(JsonHelper));
        handlebars.register_helper("default", Box::new(DefaultHelper));
        handlebars.register_helper("ifEq", Box::new(IfEqHelper));
        Self {
            handlebars,
            manifest_id,
        }
    }

    pub fn render_json(
        &self,
        template: &str,
        state: &Map<String, Value>,
        node_id: &str,
    ) -> Result<Value> {
        let preprocessed = preprocess_template(template);
        let mut ctx = Map::new();
        ctx.insert("state".to_string(), Value::Object(state.clone()));
        let rendered = self
            .handlebars
            .render_template(&preprocessed, &ctx)
            .map_err(|e| FlowError::Internal {
                message: format!(
                    "template render error in node '{node_id}'{}: {e}",
                    manifest_label(self.manifest_id.as_deref())
                ),
                location: FlowErrorLocation::at_path(format!("nodes.{node_id}.template")),
            })?;
        let mut value: Value =
            serde_json::from_str(&rendered).map_err(|e| FlowError::Internal {
                message: format!(
                    "template JSON parse error in node '{node_id}'{}: {e}",
                    manifest_label(self.manifest_id.as_deref())
                ),
                location: FlowErrorLocation::at_path(format!("nodes.{node_id}.template")),
            })?;
        substitute_state_tokens(&mut value, state).map_err(|e| FlowError::Internal {
            message: format!(
                "{e} (node '{node_id}'{})",
                manifest_label(self.manifest_id.as_deref())
            ),
            location: FlowErrorLocation::at_path(format!("nodes.{node_id}.template")),
        })?;
        Ok(value)
    }
}

fn manifest_label(manifest_id: Option<&str>) -> String {
    manifest_id
        .map(|id| format!(" in manifest '{id}'"))
        .unwrap_or_default()
}

fn preprocess_template(template: &str) -> String {
    let re = Regex::new(r"\{\{\s*state\.([A-Za-z_]\w*)\s*\}\}").unwrap();
    re.replace_all(template, |caps: &regex::Captures<'_>| {
        state_token_value(caps.get(1).unwrap().as_str())
    })
    .to_string()
}

fn state_token_value(key: &str) -> String {
    format!("{STATE_TOKEN_PREFIX}{key}{STATE_TOKEN_SUFFIX}")
}

fn substitute_state_tokens(
    target: &mut Value,
    state: &Map<String, Value>,
) -> std::result::Result<(), String> {
    match target {
        Value::String(s) => {
            if let Some(key) = s
                .strip_prefix(STATE_TOKEN_PREFIX)
                .and_then(|rest| rest.strip_suffix(STATE_TOKEN_SUFFIX))
            {
                let value = state
                    .get(key)
                    .ok_or_else(|| format!("state value for '{key}' not found"))?;
                *target = value.clone();
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                substitute_state_tokens(item, state)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                substitute_state_tokens(value, state)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

struct JsonHelper;

impl HelperDef for JsonHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> std::result::Result<(), RenderError> {
        let value = helper
            .param(0)
            .map(|p| p.value().clone())
            .ok_or_else(|| helper_error("json helper expects 1 parameter"))?;
        let rendered = serde_json::to_string(&value)
            .map_err(|e| helper_error(&format!("json helper: {e}")))?;
        out.write(&rendered)?;
        Ok(())
    }
}

struct DefaultHelper;

impl HelperDef for DefaultHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> std::result::Result<(), RenderError> {
        let value = helper.param(0).map(|p| p.value().clone());
        let fallback = helper
            .param(1)
            .map(|p| p.value().clone())
            .ok_or_else(|| helper_error("default helper expects 2 parameters"))?;
        let use_fallback = matches!(value.as_ref(), None | Some(Value::Null))
            || matches!(value.as_ref(), Some(Value::String(s)) if s.is_empty());
        let rendered_value = if use_fallback {
            fallback
        } else {
            value.unwrap()
        };
        let rendered = serde_json::to_string(&rendered_value)
            .map_err(|e| helper_error(&format!("default helper: {e}")))?;
        out.write(&rendered)?;
        Ok(())
    }
}

struct IfEqHelper;

impl HelperDef for IfEqHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> std::result::Result<(), RenderError> {
        let left = helper
            .param(0)
            .map(|p| p.value().clone())
            .ok_or_else(|| helper_error("ifEq helper expects 2 parameters"))?;
        let right = helper
            .param(1)
            .map(|p| p.value().clone())
            .ok_or_else(|| helper_error("ifEq helper expects 2 parameters"))?;
        let matches = left == right;
        if matches {
            if let Some(t) = helper.template() {
                t.render(r, ctx, rc, out)?;
            }
        } else if let Some(t) = helper.inverse() {
            t.render(r, ctx, rc, out)?;
        }
        Ok(())
    }
}

fn helper_error(message: &str) -> RenderError {
    RenderErrorReason::Other(message.to_string()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(template: &str, state: Map<String, Value>) -> Value {
        let renderer = TemplateRenderer::new(Some("manifest.test".to_string()));
        renderer
            .render_json(template, &state, "emit_config")
            .unwrap()
    }

    #[test]
    fn if_truthy_renders_block() {
        let mut state = Map::new();
        state.insert("needs_interaction".to_string(), Value::Bool(true));
        let template = r#"{ "enabled": {{#if state.needs_interaction}}true{{/if}} }"#;
        let value = render(template, state);
        assert_eq!(value.get("enabled"), Some(&Value::Bool(true)));
    }

    #[test]
    fn ifeq_matches_string_and_bool() {
        let mut state = Map::new();
        state.insert("mode".to_string(), Value::String("asset".to_string()));
        state.insert("flag".to_string(), Value::Bool(false));
        let template = r#"
        {
          "mode": {{#ifEq state.mode "asset"}} "asset" {{else}} "inline" {{/ifEq}},
          "flagged": {{#ifEq state.flag false}} true {{else}} false {{/ifEq}}
        }"#;
        let value = render(template, state);
        assert_eq!(value.get("mode"), Some(&Value::String("asset".to_string())));
        assert_eq!(value.get("flagged"), Some(&Value::Bool(true)));
    }

    #[test]
    fn json_helper_emits_raw_json() {
        let mut state = Map::new();
        state.insert("inline_json".to_string(), json!({"a": 1, "b": [true]}));
        let template = r#"{ "inline": {{json state.inline_json}} }"#;
        let value = render(template, state);
        assert_eq!(value.get("inline"), Some(&json!({"a": 1, "b": [true]})));
    }

    #[test]
    fn preserves_simple_state_interpolation() {
        let mut state = Map::new();
        state.insert("temperature".to_string(), json!(0.4));
        let template = r#"{ "temperature": "{{state.temperature}}" }"#;
        let value = render(template, state);
        assert_eq!(value.get("temperature"), Some(&json!(0.4)));
    }
}
