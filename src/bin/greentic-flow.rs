use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const EMBEDDED_FLOW_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schemas/ygtc.flow.schema.json"
));

use greentic_distributor_client::DistClient;
use greentic_flow::{
    add_step::{
        AddStepSpec, apply_and_validate,
        modes::{AddStepModeInput, materialize_node},
        plan_add_step,
    },
    component_catalog::ManifestCatalog,
    error::FlowError,
    flow_bundle::{FlowBundle, load_and_validate_bundle_with_schema_text},
    flow_ir::FlowIr,
    json_output::LintJsonOutput,
    lint::{lint_builtin_rules, lint_with_registry},
    loader::{load_ygtc_from_path, load_ygtc_from_str},
    registry::AdapterCatalog,
};
use greentic_types::flow_resolve::{
    ComponentSourceRefV1, FLOW_RESOLVE_SCHEMA_VERSION, FlowResolveV1, NodeResolveV1, ResolveModeV1,
    read_flow_resolve, sidecar_path_for_flow, write_flow_resolve,
};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};
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
    /// Update flow metadata in-place without overwriting nodes.
    Update(UpdateArgs),
    /// Insert a step after an anchor node.
    AddStep(AddStepArgs),
    /// Update an existing node (rerun config/default with overrides).
    UpdateStep(UpdateStepArgs),
    /// Delete a node and optionally splice routing.
    DeleteStep(DeleteStepArgs),
    /// Validate flows.
    Doctor(DoctorArgs),
    /// Attach or repair a sidecar component binding without changing flow nodes.
    BindComponent(BindComponentArgs),
}

#[derive(Args, Debug)]
struct NewArgs {
    /// Path to write the new flow.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Flow identifier.
    #[arg(long = "id")]
    flow_id: String,
    /// Flow type/kind (e.g., messaging, events, component-config).
    #[arg(long = "type")]
    flow_type: String,
    /// schema_version to write (default 2).
    #[arg(long = "schema-version", default_value_t = 2)]
    schema_version: u32,
    /// Optional flow name/title.
    #[arg(long = "name")]
    name: Option<String>,
    /// Optional flow description.
    #[arg(long = "description")]
    description: Option<String>,
    /// Overwrite the file if it already exists.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    /// Path to the flow to update.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// New flow id (only when safe; see rules).
    #[arg(long = "id")]
    flow_id: Option<String>,
    /// New flow type/kind (only when flow is empty).
    #[arg(long = "type")]
    flow_type: Option<String>,
    /// Optional new schema_version (no auto-bump).
    #[arg(long = "schema-version")]
    schema_version: Option<u32>,
    /// Optional flow name/title.
    #[arg(long = "name")]
    name: Option<String>,
    /// Optional flow description.
    #[arg(long = "description")]
    description: Option<String>,
    /// Optional comma-separated tags.
    #[arg(long = "tags")]
    tags: Option<String>,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    /// Path to the flow schema JSON file.
    #[arg(long)]
    schema: Option<PathBuf>,
    /// Optional adapter catalog used for adapter_resolvable linting.
    #[arg(long)]
    registry: Option<PathBuf>,
    /// Emit a machine-readable JSON payload describing the lint result for a single flow.
    #[arg(long)]
    json: bool,
    /// Read flow YAML from stdin (requires --json).
    #[arg(long)]
    stdin: bool,
    /// Flow files or directories to lint.
    #[arg(required_unless_present = "stdin")]
    targets: Vec<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdateStepArgs {
    /// Flow file to update.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Node id to update.
    #[arg(long = "step")]
    step: String,
    /// Mode: config (default) or default.
    #[arg(long = "mode", default_value = "config", value_parser = ["config", "default"])]
    mode: String,
    /// Optional new operation name (defaults to existing op key).
    #[arg(long = "operation")]
    operation: Option<String>,
    /// Optional routing override: out|reply or JSON array.
    #[arg(long = "routing")]
    routing: Option<String>,
    /// Answers JSON/YAML string to merge with existing payload.
    #[arg(long = "answers")]
    answers: Option<String>,
    /// Answers file (JSON/YAML) to merge with existing payload.
    #[arg(long = "answers-file")]
    answers_file: Option<PathBuf>,
    /// Non-interactive mode (merge answers/prefill; fail if required missing).
    #[arg(long = "non-interactive")]
    non_interactive: bool,
    /// Optional explicit component id (not yet used; reserved for future resolution).
    #[arg(long = "component")]
    component: Option<String>,
    /// Show the updated flow without writing it.
    #[arg(long = "dry-run")]
    dry_run: bool,
    /// Backward-compatible write flag (ignored; writing is default).
    #[arg(long = "write", hide = true)]
    write: bool,
}

#[derive(Args, Debug, Clone)]
struct DeleteStepArgs {
    /// Flow file to update.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Node id to delete.
    #[arg(long = "step")]
    step: String,
    /// Strategy: splice (default) or remove-only.
    #[arg(long = "strategy", default_value = "splice", value_parser = ["splice", "remove-only"])]
    strategy: String,
    /// Behavior when multiple predecessors are present.
    #[arg(
        long = "if-multiple-predecessors",
        default_value = "error",
        value_parser = ["error", "splice-all"]
    )]
    multi_pred: String,
    /// Skip confirmation prompt.
    #[arg(long = "assume-yes")]
    assume_yes: bool,
    /// Write back to the flow file instead of stdout.
    #[arg(long = "write")]
    write: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::New(args) => handle_new(args),
        Commands::Update(args) => handle_update(args),
        Commands::AddStep(args) => handle_add_step(args),
        Commands::UpdateStep(args) => handle_update_step(args),
        Commands::DeleteStep(args) => handle_delete_step(args),
        Commands::Doctor(args) => handle_doctor(args),
        Commands::BindComponent(args) => handle_bind_component(args),
    }
}

fn handle_doctor(args: DoctorArgs) -> Result<()> {
    if args.stdin && !args.json {
        anyhow::bail!("--stdin currently requires --json");
    }
    if args.stdin && !args.targets.is_empty() {
        anyhow::bail!("--stdin cannot be combined with file targets");
    }

    let (schema_text, schema_label, schema_path) = if let Some(schema_path) = &args.schema {
        let text = fs::read_to_string(schema_path)
            .with_context(|| format!("failed to read schema {}", schema_path.display()))?;
        (text, schema_path.display().to_string(), schema_path.clone())
    } else {
        (
            EMBEDDED_FLOW_SCHEMA.to_string(),
            "embedded ygtc.flow.schema.json".to_string(),
            PathBuf::from("schemas/ygtc.flow.schema.json"),
        )
    };

    let registry = if let Some(path) = &args.registry {
        Some(AdapterCatalog::load_from_file(path)?)
    } else {
        None
    };

    if args.json {
        let stdin_content = if args.stdin {
            Some(read_stdin_flow()?)
        } else {
            None
        };
        return run_json(
            &args.targets,
            stdin_content,
            &schema_text,
            &schema_label,
            &schema_path,
            registry.as_ref(),
        );
    }

    let mut failures = 0usize;
    for target in &args.targets {
        lint_path(
            target,
            &schema_text,
            &schema_label,
            &schema_path,
            registry.as_ref(),
            &mut failures,
        )?;
    }

    if failures == 0 {
        println!("All flows valid");
        Ok(())
    } else {
        Err(anyhow::anyhow!("{failures} flow(s) failed validation"))
    }
}

fn handle_new(args: NewArgs) -> Result<()> {
    let doc = greentic_flow::model::FlowDoc {
        id: args.flow_id.clone(),
        title: args.name,
        description: args.description,
        flow_type: args.flow_type.clone(),
        start: None,
        parameters: serde_json::Value::Object(Default::default()),
        tags: Vec::new(),
        schema_version: Some(args.schema_version),
        entrypoints: IndexMap::new(),
        nodes: IndexMap::new(),
    };
    let mut yaml = serde_yaml_bw::to_string(&doc)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    write_flow_file(&args.flow_path, &yaml, args.force)?;
    println!(
        "Created flow '{}' at {} (type: {})",
        args.flow_id,
        args.flow_path.display(),
        args.flow_type
    );
    Ok(())
}

fn handle_update(args: UpdateArgs) -> Result<()> {
    if !args.flow_path.exists() {
        anyhow::bail!(
            "flow file {} not found; use `greentic-flow new` to create it",
            args.flow_path.display()
        );
    }
    let mut doc = load_ygtc_from_path(&args.flow_path)?;

    if let Some(id) = args.flow_id {
        doc.id = id;
    }

    if let Some(name) = args.name {
        doc.title = Some(name);
    }

    if let Some(desc) = args.description {
        doc.description = Some(desc);
    }

    if let Some(tags_raw) = args.tags {
        let tags = tags_raw
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        doc.tags = tags;
    }

    if let Some(schema_version) = args.schema_version {
        doc.schema_version = Some(schema_version);
    }

    if let Some(flow_type) = args.flow_type {
        let is_empty_flow =
            doc.nodes.is_empty() && doc.entrypoints.is_empty() && doc.start.is_none();
        if !is_empty_flow {
            anyhow::bail!(
                "refusing to change type on a non-empty flow; create a new flow or migrate explicitly"
            );
        }
        doc.flow_type = flow_type;
    }

    let yaml = serialize_doc(&doc)?;
    // Validate final doc to catch accidental schema violations.
    load_ygtc_from_str(&yaml)?;
    write_flow_file(&args.flow_path, &yaml, true)?;
    println!("Updated flow metadata at {}", args.flow_path.display());
    Ok(())
}

fn lint_path(
    path: &Path,
    schema_text: &str,
    schema_label: &str,
    schema_path: &Path,
    registry: Option<&AdapterCatalog>,
    failures: &mut usize,
) -> Result<()> {
    if path.is_file() {
        lint_file(
            path,
            schema_text,
            schema_label,
            schema_path,
            registry,
            failures,
        )?;
    } else if path.is_dir() {
        let entries = fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?;
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read directory entry in {}", path.display()))?;
            lint_path(
                &entry.path(),
                schema_text,
                schema_label,
                schema_path,
                registry,
                failures,
            )?;
        }
    }
    Ok(())
}

fn lint_file(
    path: &Path,
    schema_text: &str,
    schema_label: &str,
    schema_path: &Path,
    registry: Option<&AdapterCatalog>,
    failures: &mut usize,
) -> Result<()> {
    if path.extension() != Some(OsStr::new("ygtc")) {
        return Ok(());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    match lint_flow(
        &content,
        Some(path),
        schema_text,
        schema_label,
        schema_path,
        registry,
    ) {
        Ok(result) => {
            if result.lint_errors.is_empty() {
                println!("OK  {} ({})", path.display(), result.bundle.id);
            } else {
                *failures += 1;
                eprintln!("ERR {}:", path.display());
                for err in result.lint_errors {
                    eprintln!("  {err}");
                }
            }
        }
        Err(err) => {
            *failures += 1;
            eprintln!("ERR {}: {err}", path.display());
        }
    }
    Ok(())
}

struct LintResult {
    bundle: FlowBundle,
    lint_errors: Vec<String>,
}

#[allow(clippy::result_large_err)]
fn lint_flow(
    content: &str,
    source_path: Option<&Path>,
    schema_text: &str,
    schema_label: &str,
    schema_path: &Path,
    registry: Option<&AdapterCatalog>,
) -> Result<LintResult, FlowError> {
    let (bundle, flow) = load_and_validate_bundle_with_schema_text(
        content,
        schema_text,
        schema_label.to_string(),
        Some(schema_path),
        source_path,
    )?;
    let lint_errors = if let Some(cat) = registry {
        lint_with_registry(&flow, cat)
    } else {
        lint_builtin_rules(&flow)
    };
    Ok(LintResult {
        bundle,
        lint_errors,
    })
}

fn run_json(
    targets: &[PathBuf],
    stdin_content: Option<String>,
    schema_text: &str,
    schema_label: &str,
    schema_path: &Path,
    registry: Option<&AdapterCatalog>,
) -> Result<()> {
    let (content, source_display, source_path) = if let Some(stdin_flow) = stdin_content {
        (
            stdin_flow,
            "<stdin>".to_string(),
            Some(Path::new("<stdin>")),
        )
    } else {
        if targets.len() != 1 {
            anyhow::bail!("--json mode expects exactly one target file");
        }
        let target = &targets[0];
        if target.is_dir() {
            anyhow::bail!(
                "--json target must be a file, found directory {}",
                target.display()
            );
        }
        if target.extension() != Some(OsStr::new("ygtc")) {
            anyhow::bail!("--json target must be a .ygtc file");
        }
        let content = fs::read_to_string(target)
            .with_context(|| format!("failed to read {}", target.display()))?;
        (
            content,
            target.display().to_string(),
            Some(target.as_path()),
        )
    };

    let output = match lint_flow(
        &content,
        source_path,
        schema_text,
        schema_label,
        schema_path,
        registry,
    ) {
        Ok(result) => {
            if result.lint_errors.is_empty() {
                LintJsonOutput::success(result.bundle)
            } else {
                LintJsonOutput::lint_failure(result.lint_errors, Some(source_display.clone()))
            }
        }
        Err(err) => LintJsonOutput::error(err),
    };

    let ok = output.ok;
    let line = output.into_string();
    write_stdout_line(&line)?;
    if ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!("validation failed"))
    }
}

fn read_stdin_flow() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read flow YAML from stdin")?;
    Ok(buf)
}

fn write_stdout_line(line: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match writeln!(stdout, "{line}") {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
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
    /// Show the updated flow without writing it.
    #[arg(long = "dry-run")]
    dry_run: bool,
    /// Backward-compatible write flag (ignored; writing is default).
    #[arg(long = "write", hide = true)]
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
    /// Remote component reference (oci://, repo://, store://, etc.) for sidecar binding.
    #[arg(long = "component")]
    component_ref: Option<String>,
    /// Local wasm path for sidecar binding (relative to the flow file).
    #[arg(long = "local-wasm")]
    local_wasm: Option<PathBuf>,
    /// Pin the component (resolve tag to digest or hash local wasm).
    #[arg(long = "pin")]
    pin: bool,
}

#[derive(Args, Debug)]
struct BindComponentArgs {
    /// Path to the flow file to modify.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Node id to bind.
    #[arg(long = "step")]
    step: String,
    /// Remote component reference (oci://, repo://, store://, etc.).
    #[arg(long = "component")]
    component_ref: Option<String>,
    /// Local wasm path (relative to the flow file).
    #[arg(long = "local-wasm")]
    local_wasm: Option<PathBuf>,
    /// Pin the component (resolve tag to digest or hash local wasm).
    #[arg(long = "pin")]
    pin: bool,
    /// Write back to the sidecar.
    #[arg(long = "write")]
    write: bool,
}

fn handle_add_step(args: AddStepArgs) -> Result<()> {
    let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
    let (component_source, resolve_mode) = resolve_component_source_inputs(
        args.local_wasm.as_ref(),
        args.component_ref.as_ref(),
        args.pin,
        &args.flow_path,
    )?;
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;

    let catalog = ManifestCatalog::load_from_paths(&args.manifests);

    let mode_input = match args.mode {
        AddStepMode::Default => {
            let operation = args.operation.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "--operation is required in default mode (component id is not stored in flows)"
                )
            })?;
            let payload_json: serde_json::Value =
                serde_json::from_str(&args.payload).context("parse --payload as JSON")?;
            let routing_json = if let Some(r) = args.routing.as_ref() {
                if r == "out" || r == "reply" {
                    Some(serde_json::Value::String(r.clone()))
                } else {
                    Some(
                        serde_json::from_str(r)
                            .context("parse --routing as JSON array of routes")?,
                    )
                }
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
    let inserted_id = plan.new_node.id.clone();
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

    if !args.dry_run {
        let tmp_path = args.flow_path.with_extension("ygtc.tmp");
        fs::write(&tmp_path, &output).with_context(|| format!("write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &args.flow_path)
            .with_context(|| format!("replace {}", args.flow_path.display()))?;
        sidecar.nodes.insert(
            inserted_id.clone(),
            NodeResolveV1 {
                source: component_source,
                mode: resolve_mode,
            },
        );
        write_sidecar(&sidecar_path, &sidecar)?;
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

fn handle_update_step(args: UpdateStepArgs) -> Result<()> {
    let (_sidecar_path, sidecar) = ensure_sidecar(&args.flow_path)?;
    let sidecar_entry = sidecar.nodes.get(&args.step).ok_or_else(|| {
        anyhow::anyhow!(
            "no sidecar mapping for node '{}'; run greentic-flow bind-component or re-add the step with --component/--local-wasm",
            args.step
        )
    })?;
    let component_payload = load_component_payload(&sidecar_entry.source, &args.flow_path)?;
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let mut flow_ir = FlowIr::from_doc(doc)?;
    let mut node = flow_ir
        .nodes
        .get(&args.step)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("step '{}' not found", args.step))?;

    let answers = parse_answers_inputs(args.answers.as_deref(), args.answers_file.as_deref())?;
    let mut merged_payload = node.payload.clone();
    if let Some(component_defaults) = component_payload {
        merged_payload = merge_payload(merged_payload, Some(component_defaults));
    }
    let new_payload = if args.mode == "config" || args.mode == "default" {
        merge_payload(merged_payload, answers)
    } else {
        merged_payload
    };
    let new_operation = args
        .operation
        .clone()
        .unwrap_or_else(|| node.operation.clone());
    let new_routing = if let Some(r) = args.routing.as_ref() {
        parse_routing_arg(r)?
    } else {
        node.routing.clone()
    };

    node.operation = new_operation;
    node.payload = new_payload;
    node.routing = new_routing;
    flow_ir.nodes.insert(args.step.clone(), node);

    let doc_out = flow_ir.to_doc()?;
    // Adjust entrypoint if it targeted the removed node in other ops; here node stays, so no-op.
    let yaml = serialize_doc(&doc_out)?;
    load_ygtc_from_str(&yaml)?; // schema validation
    if !args.dry_run {
        write_flow_file(&args.flow_path, &yaml, true)?;
        println!(
            "Updated step '{}' in {}",
            args.step,
            args.flow_path.display()
        );
    } else {
        print!("{yaml}");
    }
    Ok(())
}

fn handle_delete_step(args: DeleteStepArgs) -> Result<()> {
    let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let mut flow_ir = FlowIr::from_doc(doc)?;
    let target = args.step.clone();
    let target_node = flow_ir
        .nodes
        .get(&target)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("step '{}' not found", target))?;

    let mut predecessors = Vec::new();
    for (id, node) in &flow_ir.nodes {
        if node
            .routing
            .iter()
            .any(|r| r.to.as_deref() == Some(target.as_str()))
        {
            predecessors.push(id.clone());
        }
    }

    if predecessors.len() > 1 && args.multi_pred == "error" {
        anyhow::bail!(
            "multiple predecessors for '{}': {} (use --if-multiple-predecessors splice-all)",
            target,
            predecessors.join(", ")
        );
    }

    if args.strategy == "splice" {
        for pred_id in predecessors {
            if let Some(pred) = flow_ir.nodes.get_mut(&pred_id) {
                let mut new_routes = Vec::new();
                for route in &pred.routing {
                    if route.to.as_deref() == Some(target.as_str()) {
                        if target_node.routing.is_empty()
                            || target_node
                                .routing
                                .iter()
                                .all(|r| r.to.is_none() && (r.out || r.reply))
                        {
                            // drop this edge; terminal target
                            continue;
                        } else {
                            new_routes.extend(target_node.routing.clone());
                            continue;
                        }
                    }
                    new_routes.push(route.clone());
                }
                pred.routing = new_routes;
            }
        }
    }

    flow_ir.nodes.swap_remove(&target);
    // Fix entrypoint if it pointed to deleted node.
    let mut new_entrypoints = flow_ir.entrypoints.clone();
    for (_, v) in new_entrypoints.iter_mut() {
        if v == &target {
            if let Some(first) = flow_ir.nodes.keys().next() {
                *v = first.clone();
            } else {
                *v = String::new();
            }
        }
    }
    flow_ir.entrypoints = new_entrypoints;

    let doc_out = flow_ir.to_doc()?;
    let yaml = serialize_doc(&doc_out)?;
    load_ygtc_from_str(&yaml)?;
    if args.write {
        write_flow_file(&args.flow_path, &yaml, true)?;
        sidecar.nodes.remove(&target);
        write_sidecar(&sidecar_path, &sidecar)?;
        println!(
            "Deleted step '{}' from {}",
            target,
            args.flow_path.display()
        );
    } else {
        print!("{yaml}");
    }
    Ok(())
}

fn handle_bind_component(args: BindComponentArgs) -> Result<()> {
    if !args.flow_path.exists() {
        anyhow::bail!(
            "flow file {} not found; bind-component requires an existing flow",
            args.flow_path.display()
        );
    }
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    if !flow_ir.nodes.contains_key(&args.step) {
        anyhow::bail!("node '{}' not found in flow", args.step);
    }
    let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
    let (source, mode) = resolve_component_source_inputs(
        args.local_wasm.as_ref(),
        args.component_ref.as_ref(),
        args.pin,
        &args.flow_path,
    )?;
    sidecar
        .nodes
        .insert(args.step.clone(), NodeResolveV1 { source, mode });
    if args.write {
        write_sidecar(&sidecar_path, &sidecar)?;
        println!(
            "Bound component for node '{}' in {}",
            args.step,
            sidecar_path.display()
        );
    } else {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &sidecar)?;
        writeln!(stdout)?;
    }
    Ok(())
}

fn parse_answers_inputs(
    answers: Option<&str>,
    answers_file: Option<&Path>,
) -> Result<Option<serde_json::Value>> {
    let mut merged: Option<serde_json::Value> = None;
    if let Some(text) = answers {
        let parsed: serde_json::Value = serde_yaml_bw::from_str(text)
            .or_else(|_| serde_json::from_str(text))
            .context("parse --answers as JSON/YAML")?;
        merged = Some(merge_payload(
            merged.unwrap_or(serde_json::Value::Null),
            Some(parsed),
        ));
    }
    if let Some(path) = answers_file {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read answers file {}", path.display()))?;
        let parsed: serde_json::Value = serde_yaml_bw::from_str(&text)
            .or_else(|_| serde_json::from_str(&text))
            .context("parse answers file as JSON/YAML")?;
        merged = Some(merge_payload(
            merged.unwrap_or(serde_json::Value::Null),
            Some(parsed),
        ));
    }
    Ok(merged)
}

fn merge_payload(base: serde_json::Value, overlay: Option<serde_json::Value>) -> serde_json::Value {
    let Some(overlay) = overlay else { return base };
    match (base, overlay) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                b.insert(k, v);
            }
            serde_json::Value::Object(b)
        }
        (_, other) => other,
    }
}

fn parse_routing_arg(raw: &str) -> Result<Vec<greentic_flow::flow_ir::Route>> {
    if raw == "out" {
        return Ok(vec![greentic_flow::flow_ir::Route {
            out: true,
            ..Default::default()
        }]);
    }
    if raw == "reply" {
        return Ok(vec![greentic_flow::flow_ir::Route {
            reply: true,
            ..Default::default()
        }]);
    }
    let routes: Vec<greentic_flow::flow_ir::Route> =
        serde_json::from_str(raw).context("parse --routing as JSON array or shorthand string")?;
    Ok(routes)
}

fn serialize_doc(doc: &greentic_flow::model::FlowDoc) -> Result<String> {
    let mut yaml = serde_yaml_bw::to_string(doc)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml)
}

fn ensure_sidecar(flow_path: &Path) -> Result<(PathBuf, FlowResolveV1)> {
    let sidecar_path = sidecar_path_for_flow(flow_path);
    if sidecar_path.exists() {
        let doc = read_flow_resolve(&sidecar_path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        return Ok((sidecar_path, doc));
    }
    let flow_name = flow_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "flow.ygtc".to_string());
    let doc = FlowResolveV1 {
        schema_version: FLOW_RESOLVE_SCHEMA_VERSION,
        flow: flow_name,
        nodes: Default::default(),
    };
    write_sidecar(&sidecar_path, &doc)?;
    Ok((sidecar_path, doc))
}

fn write_sidecar(path: &Path, doc: &FlowResolveV1) -> Result<()> {
    write_flow_resolve(path, doc).map_err(|e| anyhow::anyhow!(e.to_string()))
}

fn classify_remote_source(reference: &str, digest: Option<String>) -> ComponentSourceRefV1 {
    if reference.starts_with("repo://") {
        ComponentSourceRefV1::Repo {
            r#ref: reference.to_string(),
            digest,
        }
    } else if reference.starts_with("store://") {
        ComponentSourceRefV1::Store {
            r#ref: reference.to_string(),
            digest,
            license_hint: None,
            meter: None,
        }
    } else {
        ComponentSourceRefV1::Oci {
            r#ref: reference.to_string(),
            digest,
        }
    }
}

fn compute_local_digest(path: &Path) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("read wasm at {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = format!("sha256:{:x}", hasher.finalize());
    Ok(digest)
}

fn resolve_remote_digest(reference: &str) -> Result<String> {
    if let Ok(mock) = std::env::var("GREENTIC_FLOW_TEST_DIGEST")
        && !mock.is_empty()
    {
        return Ok(mock);
    }
    let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let client = DistClient::new(Default::default());
    let resolved = rt
        .block_on(client.resolve_ref(reference))
        .map_err(|e| anyhow::anyhow!("failed to resolve reference {reference}: {e}"))?;
    Ok(resolved.digest)
}

fn resolve_component_source_inputs(
    local_wasm: Option<&PathBuf>,
    component_ref: Option<&String>,
    pin: bool,
    flow_path: &Path,
) -> Result<(ComponentSourceRefV1, Option<ResolveModeV1>)> {
    if let Some(local) = local_wasm {
        if local.is_absolute() {
            anyhow::bail!("--local-wasm must be a relative path to the flow file");
        }
        let digest = if pin {
            Some(compute_local_digest(
                &flow_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(local),
            )?)
        } else {
            None
        };
        let source = ComponentSourceRefV1::Local {
            path: local.to_string_lossy().to_string(),
            digest: digest.clone(),
        };
        let mode = digest.as_ref().map(|_| ResolveModeV1::Pinned);
        return Ok((source, mode));
    }

    if let Some(reference) = component_ref {
        let digest = if pin {
            Some(resolve_remote_digest(reference)?)
        } else {
            None
        };
        let source = classify_remote_source(reference, digest.clone());
        let mode = digest.as_ref().map(|_| ResolveModeV1::Pinned);
        return Ok((source, mode));
    }

    anyhow::bail!("component source is required; provide --component <ref> or --local-wasm <path>");
}

fn ensure_sidecar_source_available(source: &ComponentSourceRefV1, flow_path: &Path) -> Result<()> {
    match source {
        ComponentSourceRefV1::Local { path, .. } => {
            let abs = flow_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path);
            if !abs.exists() {
                anyhow::bail!(
                    "local wasm for node missing at {}; rebuild component or update sidecar",
                    abs.display()
                );
            }
        }
        ComponentSourceRefV1::Oci { r#ref, digest }
        | ComponentSourceRefV1::Repo { r#ref, digest }
        | ComponentSourceRefV1::Store { r#ref, digest, .. } => {
            let client = DistClient::new(Default::default());
            let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
            if let Some(d) = digest {
                rt.block_on(client.fetch_digest(d)).map_err(|e| {
                    anyhow::anyhow!(
                        "component digest {} not cached; pull or pin locally first: {e}",
                        d
                    )
                })?;
            } else {
                rt.block_on(client.ensure_cached(r#ref)).map_err(|e| {
                    anyhow::anyhow!(
                        "component reference {} not available locally; pull or pin digest: {e}",
                        r#ref
                    )
                })?;
            }
        }
    }
    Ok(())
}

fn load_component_payload(
    source: &ComponentSourceRefV1,
    flow_path: &Path,
) -> Result<Option<serde_json::Value>> {
    ensure_sidecar_source_available(source, flow_path)?;
    let manifest_path = match source {
        ComponentSourceRefV1::Local { path, .. } => flow_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
            .parent()
            .map(|p| p.join("component.manifest.json"))
            .unwrap_or_else(|| {
                flow_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("component.manifest.json")
            }),
        ComponentSourceRefV1::Oci { r#ref, digest }
        | ComponentSourceRefV1::Repo { r#ref, digest }
        | ComponentSourceRefV1::Store { r#ref, digest, .. } => {
            let client = DistClient::new(Default::default());
            let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
            let artifact = if let Some(d) = digest {
                rt.block_on(client.fetch_digest(d))
            } else {
                rt.block_on(client.ensure_cached(r#ref))
                    .map(|r| r.cache_path.unwrap_or_default())
            }
            .map_err(|e| anyhow::anyhow!("resolve component {}: {e}", r#ref))?;
            artifact
                .parent()
                .map(|p| p.join("component.manifest.json"))
                .unwrap_or_else(|| PathBuf::from("component.manifest.json"))
        }
    };

    if !manifest_path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).context("parse manifest JSON for defaults")?;
    if let Some(props) = json
        .get("config_schema")
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
    {
        let mut defaults = serde_json::Map::new();
        for (k, v) in props {
            if let Some(def) = v.get("default") {
                defaults.insert(k.clone(), def.clone());
            }
        }
        if !defaults.is_empty() {
            return Ok(Some(serde_json::Value::Object(defaults)));
        }
    }
    Ok(None)
}
