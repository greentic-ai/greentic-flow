use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use indexmap::IndexMap;
use pathdiff::diff_paths;
use serde::Serialize;
use serde_yaml_bw::Value as YamlValue;
use std::{
    fs,
    path::{Path, PathBuf},
};

use greentic_flow::{
    add_step::{
        AddStepSpec, apply_and_validate,
        modes::{AddStepModeInput, materialize_node},
        plan_add_step,
    },
    component_catalog::ManifestCatalog,
    flow_ir::FlowIr,
    loader::load_ygtc_from_path,
};
#[derive(Parser, Debug)]
#[command(name = "greentic-flow", about = "Flow scaffolding helpers")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new flow skeleton at the given path.
    New(NewArgs),
    /// Insert a step after an anchor node.
    AddStep(AddStepArgs),
}

#[derive(Args, Debug)]
struct NewArgs {
    /// Path to write the new flow (e.g., flows/my_flow.ygtc).
    #[arg(value_name = "PATH")]
    path: PathBuf,
    /// Flow kind: messaging, events, or deployment (deployment is sugar for events).
    #[arg(long, value_enum)]
    kind: Option<FlowKind>,
    /// Alias for --kind deployment.
    #[arg(long)]
    deployment: bool,
    /// Flow identifier; defaults to the file stem.
    #[arg(long = "id")]
    flow_id: Option<String>,
    /// Optional flow description shown at the top of the file.
    #[arg(long)]
    description: Option<String>,
    /// Overwrite the file if it already exists.
    #[arg(long)]
    force: bool,
    /// Optional manifest path for detecting pack.kind (defaults to ./manifest.yaml if present).
    #[arg(long = "pack-manifest")]
    manifest_path: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum FlowKind {
    Messaging,
    Events,
    Deployment,
}

impl FlowKind {
    fn effective_flow_type(self) -> &'static str {
        match self {
            FlowKind::Messaging => "messaging",
            FlowKind::Events | FlowKind::Deployment => "events",
        }
    }

    fn default_description(self) -> &'static str {
        match self {
            FlowKind::Messaging => "Describe what this messaging flow should accomplish.",
            FlowKind::Events => "Describe the event trigger and the action performed.",
            FlowKind::Deployment => {
                "Render infrastructure-as-code artifacts for the current DeploymentPlan."
            }
        }
    }
}

#[derive(Serialize)]
struct FlowScaffold {
    id: String,
    #[serde(rename = "type")]
    flow_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    nodes: IndexMap<String, NodeTemplate>,
}

#[derive(Serialize)]
struct NodeTemplate {
    #[serde(flatten)]
    component: IndexMap<String, YamlValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing: Option<Vec<RouteTemplate>>,
}

#[derive(Serialize)]
struct RouteTemplate {
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    out: Option<bool>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::New(args) => handle_new(args),
        Commands::AddStep(args) => handle_add_step(args),
    }
}

fn handle_new(mut args: NewArgs) -> Result<()> {
    let mut manifest = load_manifest(args.manifest_path.as_deref())?;
    if args.deployment {
        args.kind = Some(FlowKind::Deployment);
    }

    let manifest_kind = manifest.as_ref().and_then(|info| info.kind_lower());

    let flow_kind = args
        .kind
        .or(match manifest_kind.as_deref() {
            Some("deployment") => Some(FlowKind::Deployment),
            Some("events") => Some(FlowKind::Events),
            _ => None,
        })
        .unwrap_or(FlowKind::Messaging);

    let id = args
        .flow_id
        .or_else(|| derive_id_from_path(&args.path))
        .unwrap_or_else(|| "new_flow".to_string());

    let description = args
        .description
        .filter(|d| !d.trim().is_empty())
        .map(|d| d.trim().to_string())
        .unwrap_or_else(|| flow_kind.default_description().to_string());

    let should_warn = manifest_kind.as_deref() == Some("deployment")
        && flow_kind.effective_flow_type() != "events";

    let yaml = render_flow_yaml(&id, &description, flow_kind)?;

    write_flow_file(&args.path, &yaml, args.force)?;

    println!(
        "Created {} flow '{}' at {}",
        flow_kind.effective_flow_type(),
        id,
        args.path.display()
    );

    if should_warn {
        eprintln!(
            "info: pack is marked kind: deployment but flow '{}' uses type: {}",
            id,
            flow_kind.effective_flow_type()
        );
    }

    if let Some(manifest) = manifest.as_mut() {
        register_flow_in_manifest(manifest, &id, &args.path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_config_flow;
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    #[test]
    fn resolves_default_config_flow_from_manifest() {
        let manifest = json!({
            "id": "ai.greentic.hello",
            "dev_flows": {
                "default": {
                    "graph": {
                        "id": "cfg",
                        "type": "component-config",
                        "nodes": {}
                    }
                }
            }
        });
        let manifest_file = NamedTempFile::new().expect("temp file");
        std::fs::write(manifest_file.path(), manifest.to_string()).expect("write manifest");

        let (yaml, schema_path) =
            resolve_config_flow(None, &[manifest_file.path().to_path_buf()]).expect("resolve");
        assert!(yaml.contains("id: cfg"));
        assert_eq!(
            schema_path,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/ygtc.flow.schema.json")
        );
    }
}
fn write_flow_file(path: &Path, content: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "refusing to overwrite existing file {}; pass --force to replace it",
            path.display()
        );
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory {}", parent.display()))?;
    }

    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn derive_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    if stem.trim().is_empty() {
        None
    } else {
        Some(stem.replace(' ', "_"))
    }
}

fn render_flow_yaml(id: &str, description: &str, kind: FlowKind) -> Result<String> {
    let nodes = match kind {
        FlowKind::Messaging => messaging_nodes(),
        FlowKind::Events => events_nodes(),
        FlowKind::Deployment => deployment_nodes(),
    };

    let scaffold = FlowScaffold {
        id: id.to_string(),
        flow_type: kind.effective_flow_type().to_string(),
        description: Some(description.to_string()),
        nodes,
    };

    let mut yaml = serde_yaml_bw::to_string(&scaffold)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml)
}

fn resolve_config_flow(
    config_flow_arg: Option<PathBuf>,
    manifests: &[PathBuf],
) -> Result<(String, PathBuf)> {
    if let Some(path) = config_flow_arg {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read config flow {}", path.display()))?;
        return Ok((text, path));
    }

    let manifest_path = manifests.first().ok_or_else(|| {
        anyhow::anyhow!(
            "config mode requires --config-flow or at least one --manifest with dev_flows.default"
        )
    })?;
    let manifest_text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let manifest_json: serde_json::Value =
        serde_json::from_str(&manifest_text).context("parse manifest JSON")?;
    let default_graph = manifest_json
        .get("dev_flows")
        .and_then(|v| v.get("default"))
        .and_then(|v| v.get("graph"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("manifest missing dev_flows.default.graph"))?;
    let mut graph = default_graph;
    if let Some(obj) = graph.as_object_mut()
        && !obj.contains_key("type")
    {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("component-config".to_string()),
        );
    }
    let yaml =
        serde_yaml_bw::to_string(&graph).context("render dev_flows.default.graph to YAML")?;
    // Use repo-local schema path as a reasonable default (absolute to avoid cwd issues).
    let schema_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/ygtc.flow.schema.json");
    Ok((yaml, schema_path))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum AddStepMode {
    Default,
    Config,
}

#[derive(Args, Debug)]
struct AddStepArgs {
    /// Path to the flow file to modify.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Optional anchor node id; defaults to entrypoint or first node.
    #[arg(long = "after")]
    after: Option<String>,
    /// How to source the node to insert.
    #[arg(long = "mode", value_enum)]
    mode: AddStepMode,
    /// Component id (default mode).
    #[arg(long = "component")]
    component_id: Option<String>,
    /// Optional pack alias for the new node.
    #[arg(long = "pack-alias")]
    pack_alias: Option<String>,
    /// Optional operation for the new node.
    #[arg(long = "operation")]
    operation: Option<String>,
    /// Payload JSON for the new node (default mode).
    #[arg(long = "payload", default_value = "{}")]
    payload: String,
    /// Optional routing JSON for the new node (default mode).
    #[arg(long = "routing")]
    routing: Option<String>,
    /// Config flow file to execute (config mode).
    #[arg(long = "config-flow")]
    config_flow: Option<PathBuf>,
    /// Answers JSON for config mode.
    #[arg(long = "answers")]
    answers: Option<String>,
    /// Answers file (JSON) for config mode.
    #[arg(long = "answers-file")]
    answers_file: Option<PathBuf>,
    /// Allow cycles/back-edges during insertion.
    #[arg(long = "allow-cycles")]
    allow_cycles: bool,
    /// Write back to the flow file instead of stdout.
    #[arg(long = "write")]
    write: bool,
    /// Validate only without writing output.
    #[arg(long = "validate-only")]
    validate_only: bool,
    /// Optional component manifest paths for catalog validation.
    #[arg(long = "manifest")]
    manifests: Vec<PathBuf>,
    /// Optional explicit node id hint.
    #[arg(long = "node-id")]
    node_id: Option<String>,
}

fn handle_add_step(args: AddStepArgs) -> Result<()> {
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;

    let catalog = ManifestCatalog::load_from_paths(&args.manifests);

    let mode_input = match args.mode {
        AddStepMode::Default => {
            let operation = args
                .operation
                .clone()
                .or(args.component_id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("--operation (or --component) is required in default mode")
                })?;
            let payload_json: serde_json::Value =
                serde_json::from_str(&args.payload).context("parse --payload as JSON")?;
            let routing_json = if let Some(r) = args.routing.as_ref() {
                Some(serde_json::from_str(r).context("parse --routing as JSON array of routes")?)
            } else {
                None
            };
            AddStepModeInput::Default {
                operation,
                payload: payload_json,
                routing: routing_json,
            }
        }
        AddStepMode::Config => {
            let (config_flow, schema_path) =
                resolve_config_flow(args.config_flow.clone(), &args.manifests)?;
            let mut answers = serde_json::Map::new();
            if let Some(a) = args.answers {
                let parsed: serde_json::Value =
                    serde_json::from_str(&a).context("parse --answers JSON")?;
                if let Some(obj) = parsed.as_object() {
                    answers.extend(obj.clone());
                }
            }
            if let Some(file) = args.answers_file {
                let text = fs::read_to_string(&file)
                    .with_context(|| format!("read {}", file.display()))?;
                let parsed: serde_json::Value =
                    serde_json::from_str(&text).context("parse answers file as JSON")?;
                if let Some(obj) = parsed.as_object() {
                    answers.extend(obj.clone());
                }
            }
            AddStepModeInput::Config {
                config_flow,
                schema_path: schema_path.into_boxed_path(),
                answers,
            }
        }
    };

    let (hint, node_value) = materialize_node(mode_input, &catalog)?;

    let spec = AddStepSpec {
        after: args.after.clone(),
        node_id_hint: args.node_id.or(hint),
        node: node_value,
        allow_cycles: args.allow_cycles,
    };

    let plan = plan_add_step(&flow_ir, spec, &catalog)
        .map_err(|diags| anyhow::anyhow!("planning failed: {:?}", diags))?;
    let updated = apply_and_validate(&flow_ir, plan, &catalog, args.allow_cycles)?;
    let updated_doc = updated.to_doc()?;
    let mut output = serde_yaml_bw::to_string(&updated_doc)?;
    if !output.ends_with('\n') {
        output.push('\n');
    }

    if args.validate_only {
        println!("add-step validation succeeded");
        return Ok(());
    }

    if args.write {
        let tmp_path = args.flow_path.with_extension("ygtc.tmp");
        fs::write(&tmp_path, &output).with_context(|| format!("write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &args.flow_path)
            .with_context(|| format!("replace {}", args.flow_path.display()))?;
        println!(
            "Inserted node after '{}' and wrote {}",
            args.after.unwrap_or_else(|| "<default anchor>".to_string()),
            args.flow_path.display()
        );
    } else {
        print!("{output}");
    }

    Ok(())
}

fn messaging_nodes() -> IndexMap<String, NodeTemplate> {
    let mut nodes = IndexMap::new();
    nodes.insert(
        "entry".to_string(),
        node_template(
            "component.kind.entry",
            serde_yaml_bw::to_value(indexmap::indexmap! {
                "prompt".to_string() => YamlValue::from("Start conversation or ask the first question."),
            })
            .unwrap(),
            Some(vec![route_to("step2")]),
        ),
    );
    nodes.insert(
        "step2".to_string(),
        node_template(
            "component.kind.action",
            serde_yaml_bw::to_value(indexmap::indexmap! {
                "note".to_string() => YamlValue::from("Call APIs or other components needed for the conversation."),
            })
            .unwrap(),
            Some(vec![route_out()]),
        ),
    );
    nodes
}

fn events_nodes() -> IndexMap<String, NodeTemplate> {
    let mut nodes = IndexMap::new();
    nodes.insert(
        "transform".to_string(),
        node_template(
            "component.kind.transform",
            serde_yaml_bw::to_value(indexmap::indexmap! {
                "note".to_string() => YamlValue::from("Map the inbound payload to whatever the next node expects."),
            })
            .unwrap(),
            Some(vec![route_to("action")]),
        ),
    );
    nodes.insert(
        "action".to_string(),
        node_template(
            "component.kind.action",
            serde_yaml_bw::to_value(indexmap::indexmap! {
                "config".to_string() => YamlValue::Mapping(Default::default()),
            })
            .unwrap(),
            Some(vec![route_out()]),
        ),
    );
    nodes
}

fn deployment_nodes() -> IndexMap<String, NodeTemplate> {
    let mut nodes = IndexMap::new();
    nodes.insert(
        "render".to_string(),
        node_template(
            "deploy.renderer",
            serde_yaml_bw::to_value(indexmap::indexmap! {
                "component".to_string() => YamlValue::from("your.deployment.component"),
                "profile".to_string() => YamlValue::from("iac-generator"),
                "config".to_string() => YamlValue::Mapping(Default::default()),
                "note".to_string() => YamlValue::from("Access the DeploymentPlan via greentic:deploy-plan@1.0.0."),
            })
            .unwrap(),
            Some(vec![route_to("done")]),
        ),
    );
    nodes.insert(
        "done".to_string(),
        node_template(
            "noop",
            serde_yaml_bw::to_value(indexmap::indexmap! {
                "config".to_string() => YamlValue::Mapping(Default::default()),
            })
            .unwrap(),
            Some(vec![route_out()]),
        ),
    );
    nodes
}

fn node_template(
    component_kind: &str,
    config: YamlValue,
    routing: Option<Vec<RouteTemplate>>,
) -> NodeTemplate {
    let mut component = IndexMap::new();
    component.insert(component_kind.to_string(), config);
    NodeTemplate { component, routing }
}

fn route_to(target: &str) -> RouteTemplate {
    RouteTemplate {
        to: Some(target.to_string()),
        out: None,
    }
}

fn route_out() -> RouteTemplate {
    RouteTemplate {
        to: None,
        out: Some(true),
    }
}

struct ManifestInfo {
    path: PathBuf,
    value: serde_yaml_bw::Value,
}

impl ManifestInfo {
    fn kind_lower(&self) -> Option<String> {
        self.value
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase())
    }
}

fn load_manifest(path: Option<&Path>) -> Result<Option<ManifestInfo>> {
    let (manifest_path, explicit) = if let Some(p) = path {
        (p.to_path_buf(), true)
    } else {
        let default = PathBuf::from("manifest.yaml");
        if default.exists() {
            (default, false)
        } else {
            return Ok(None);
        }
    };

    if !manifest_path.exists() {
        if explicit {
            anyhow::bail!(
                "manifest file {} not found (required by --pack-manifest)",
                manifest_path.display()
            );
        }
        return Ok(None);
    }

    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let value: serde_yaml_bw::Value =
        serde_yaml_bw::from_str(&contents).with_context(|| "failed to parse manifest")?;
    Ok(Some(ManifestInfo {
        path: manifest_path,
        value,
    }))
}

fn register_flow_in_manifest(
    manifest: &mut ManifestInfo,
    id: &str,
    flow_path: &Path,
) -> Result<()> {
    use serde_yaml_bw::{Mapping, Sequence, Value};

    let root = manifest.value.as_mapping_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "manifest {} must be a YAML mapping",
            manifest.path.display()
        )
    })?;

    let flows_value = root
        .entry(Value::from("flows"))
        .or_insert_with(|| Value::Sequence(Sequence::new()));

    let flows = flows_value.as_sequence_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "manifest {} has non-sequence flows",
            manifest.path.display()
        )
    })?;

    let already_present = flows.iter().any(|entry| match entry {
        Value::Mapping(map) => map
            .get(Value::from("id"))
            .and_then(|v| v.as_str())
            .map(|existing| existing == id)
            .unwrap_or(false),
        _ => false,
    });

    if already_present {
        return Ok(());
    }

    let manifest_dir = manifest
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let rel = diff_paths(flow_path, &manifest_dir).unwrap_or_else(|| flow_path.to_path_buf());
    let rel_string = rel.to_string_lossy().replace('\\', "/");

    let mut entry = Mapping::new();
    entry.insert(Value::from("id"), Value::from(id.to_string()));
    entry.insert(Value::from("file"), Value::from(rel_string));
    flows.push(Value::Mapping(entry));

    let serialized = serde_yaml_bw::to_string(&manifest.value)?;
    fs::write(&manifest.path, serialized)
        .with_context(|| format!("failed to write {}", manifest.path.display()))?;
    Ok(())
}
