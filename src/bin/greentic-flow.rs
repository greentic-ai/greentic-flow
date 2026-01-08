use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

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
    /// Update an existing node (rerun config/default with overrides).
    UpdateStep(UpdateStepArgs),
    /// Delete a node and optionally splice routing.
    DeleteStep(DeleteStepArgs),
    /// Validate flows.
    Doctor(DoctorArgs),
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
struct DoctorArgs {
    /// Path to the flow schema JSON file.
    #[arg(long, default_value = "schemas/ygtc.flow.schema.json")]
    schema: PathBuf,
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
    /// Write back to the flow file instead of stdout.
    #[arg(long = "write")]
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
        Commands::AddStep(args) => handle_add_step(args),
        Commands::UpdateStep(args) => handle_update_step(args),
        Commands::DeleteStep(args) => handle_delete_step(args),
        Commands::Doctor(args) => handle_doctor(args),
    }
}

fn handle_doctor(args: DoctorArgs) -> Result<()> {
    if args.stdin && !args.json {
        anyhow::bail!("--stdin currently requires --json");
    }
    if args.stdin && !args.targets.is_empty() {
        anyhow::bail!("--stdin cannot be combined with file targets");
    }

    let schema_text = fs::read_to_string(&args.schema)
        .with_context(|| format!("failed to read schema {}", args.schema.display()))?;
    let schema_label = args.schema.display().to_string();

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
            &args.schema,
            registry.as_ref(),
        );
    }

    let mut failures = 0usize;
    for target in &args.targets {
        lint_path(
            target,
            &schema_text,
            &schema_label,
            &args.schema,
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
        entrypoints: std::collections::BTreeMap::new(),
        nodes: std::collections::BTreeMap::new(),
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

fn handle_update_step(args: UpdateStepArgs) -> Result<()> {
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let mut flow_ir = FlowIr::from_doc(doc)?;
    let mut node = flow_ir
        .nodes
        .get(&args.step)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("step '{}' not found", args.step))?;

    let answers = parse_answers_inputs(args.answers.as_deref(), args.answers_file.as_deref())?;
    let new_payload = if args.mode == "config" || args.mode == "default" {
        merge_payload(node.payload.clone(), answers)
    } else {
        node.payload.clone()
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
    if args.write {
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
