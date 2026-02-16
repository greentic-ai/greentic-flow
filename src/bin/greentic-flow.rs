use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const EMBEDDED_FLOW_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schemas/ygtc.flow.schema.json"
));

use greentic_distributor_client::{
    DistClient, DistributorClient, DistributorClientConfig, DistributorEnvironmentId, EnvId,
    HttpDistributorClient, ResolveComponentRequest, TenantCtx, TenantId,
};
use greentic_flow::{
    add_step::{
        AddStepSpec, apply_and_validate,
        modes::{AddStepModeInput, materialize_node},
        normalize::normalize_node_map,
        normalize_node_id_hint, plan_add_step,
    },
    answers,
    component_catalog::ManifestCatalog,
    component_schema::{
        is_effectively_empty_schema, jsonschema_options_with_base, resolve_input_schema,
        schema_guidance, validate_payload_against_schema,
    },
    config_flow::run_config_flow,
    contracts,
    error::FlowError,
    flow_bundle::{FlowBundle, load_and_validate_bundle_with_schema_text},
    flow_ir::FlowIr,
    flow_meta,
    i18n::{I18nCatalog, resolve_locale},
    json_output::LintJsonOutput,
    lint::{lint_builtin_rules, lint_with_registry},
    loader::{ensure_config_schema_path, load_ygtc_from_path, load_ygtc_from_str},
    qa_runner,
    questions::{
        Answers as QuestionAnswers, Question, apply_writes_to, extract_answers_from_payload,
        extract_questions_from_flow, run_interactive_with_seed, validate_required,
    },
    questions_schema::{example_for_questions, schema_for_questions},
    registry::AdapterCatalog,
    resolve::resolve_parameters,
    resolve_summary::{remove_flow_resolve_summary_node, write_flow_resolve_summary_for_node},
    schema_mode::SchemaMode,
    schema_validate::{Severity, validate_value_against_schema},
    wizard_ops, wizard_state,
};
use greentic_types::flow_resolve::{
    ComponentSourceRefV1, FLOW_RESOLVE_SCHEMA_VERSION, FlowResolveV1, NodeResolveV1, ResolveModeV1,
    read_flow_resolve, sidecar_path_for_flow, write_flow_resolve,
};
use indexmap::IndexMap;
use jsonschema::error::ValidationErrorKind;
use jsonschema::{Draft, ReferencingError};
use pathdiff::diff_paths;
use serde_json::json;
use sha2::{Digest, Sha256};

fn derive_contract_meta(
    describe_cbor: &[u8],
    operation_id: &str,
) -> Result<(
    greentic_types::schemas::component::v0_6_0::ComponentDescribe,
    flow_meta::ComponentContractMeta,
)> {
    let describe = contracts::decode_component_describe(describe_cbor)?;
    let describe_hash = contracts::describe_hash(&describe)?;
    let op = contracts::find_operation(&describe, operation_id)?;
    let computed_schema_hash = contracts::recompute_schema_hash(op, &describe.config_schema)?;
    if computed_schema_hash != op.schema_hash {
        anyhow::bail!(
            "schema_hash mismatch for operation '{}': expected {}, computed {}",
            operation_id,
            op.schema_hash,
            computed_schema_hash
        );
    }
    let world = describe
        .metadata
        .get("world")
        .and_then(|v| v.as_text())
        .map(|s| s.to_string());
    let config_schema_bytes =
        greentic_types::cbor::canonical::to_canonical_cbor_allow_floats(&describe.config_schema)
            .map_err(|err| anyhow!("encode config schema: {err}"))?;
    let meta = flow_meta::ComponentContractMeta {
        describe_hash,
        operation_id: operation_id.to_string(),
        schema_hash: computed_schema_hash,
        component_version: Some(describe.info.version.clone()),
        world,
        config_schema_cbor: Some(bytes_to_hex(&config_schema_bytes)),
    };
    Ok((describe, meta))
}

fn validate_config_schema(
    describe: &greentic_types::schemas::component::v0_6_0::ComponentDescribe,
    config_cbor: &[u8],
) -> Result<()> {
    let value: ciborium::value::Value = ciborium::de::from_reader(config_cbor)
        .map_err(|err| anyhow!("decode config cbor: {err}"))?;
    let diags = validate_value_against_schema(&describe.config_schema, &value);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for diag in diags {
        match diag.severity {
            Severity::Error => errors.push(diag),
            Severity::Warning => warnings.push(diag),
        }
    }
    for warn in warnings {
        eprintln!("warning: {} ({})", warn.message, warn.code);
    }
    if !errors.is_empty() {
        let mut lines = Vec::new();
        for err in errors {
            lines.push(format!("{} ({})", err.message, err.code));
        }
        anyhow::bail!("config schema validation failed:\n{}", lines.join("\n"));
    }
    Ok(())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn default_i18n_catalog(locale: Option<&str>) -> (I18nCatalog, String) {
    let locale = resolve_locale(locale);
    (I18nCatalog::default(), locale)
}

fn answers_base_dir(flow_path: &Path, answers_dir: Option<&Path>) -> PathBuf {
    let base = flow_path.parent().unwrap_or_else(|| Path::new("."));
    let dir = answers_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("answers"));
    base.join(dir)
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    let trimmed = hex.trim();
    if !trimmed.len().is_multiple_of(2) {
        anyhow::bail!("hex payload has odd length");
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let chars: Vec<char> = trimmed.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        let hi = chars[idx];
        let lo = chars[idx + 1];
        let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16)
            .map_err(|err| anyhow!("decode hex: {err}"))?;
        out.push(byte);
        idx += 2;
    }
    Ok(out)
}
#[derive(Parser, Debug)]
#[command(name = "greentic-flow", about = "Flow scaffolding helpers", version)]
struct Cli {
    /// Enable permissive schema handling (default: strict).
    #[arg(long, global = true)]
    permissive: bool,
    /// Output format (human or json).
    #[arg(long, global = true, value_enum, default_value = "human")]
    format: OutputFormat,
    /// Diagnostic locale (BCP47).
    #[arg(long, global = true)]
    locale: Option<String>,
    /// Backup flow files before overwriting (suffix .bak).
    #[arg(long, global = true)]
    backup: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
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
    /// Validate answers JSON against a schema.
    DoctorAnswers(DoctorAnswersArgs),
    /// Emit JSON schema + example answers for a component operation.
    Answers(AnswersArgs),
    /// Attach or repair a sidecar component binding without changing flow nodes.
    BindComponent(BindComponentArgs),
    /// Wizard flow helpers (interactive by default).
    Wizard(WizardArgs),
}

#[derive(Args, Debug)]
struct WizardArgs {
    #[command(subcommand)]
    command: WizardCommand,
}

#[derive(Subcommand, Debug)]
enum WizardCommand {
    /// Create a new flow skeleton.
    New(NewArgs),
    /// Update flow metadata in-place without overwriting nodes.
    Edit(UpdateArgs),
    /// Insert a step after an anchor node (wizard mode).
    AddStep(WizardAddStepArgs),
    /// Update an existing node (wizard mode).
    UpdateStep(WizardUpdateStepArgs),
    /// Delete a node and optionally splice routing (wizard mode).
    RemoveStep(WizardRemoveStepArgs),
}

#[derive(Args, Debug)]
struct WizardAddStepArgs {
    #[command(flatten)]
    args: AddStepArgs,
    /// Disable interactive prompts (requires provided answers).
    #[arg(long = "non-interactive")]
    non_interactive: bool,
}

#[derive(Args, Debug)]
struct WizardUpdateStepArgs {
    #[command(flatten)]
    args: UpdateStepArgs,
    /// Disable interactive prompts (requires provided answers).
    #[arg(long = "non-interactive")]
    non_interactive: bool,
}

#[derive(Args, Debug)]
struct WizardRemoveStepArgs {
    #[command(flatten)]
    args: DeleteStepArgs,
    /// Disable interactive prompts (requires provided answers).
    #[arg(long = "non-interactive")]
    non_interactive: bool,
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
    /// Re-resolve components and verify contract drift (networked).
    #[arg(long)]
    online: bool,
    /// Flow files or directories to lint.
    #[arg(required_unless_present = "stdin")]
    targets: Vec<PathBuf>,
}

#[derive(Args, Debug)]
struct DoctorAnswersArgs {
    /// Path to the answers JSON schema.
    #[arg(long = "schema")]
    schema: PathBuf,
    /// Path to the answers JSON.
    #[arg(long = "answers")]
    answers: PathBuf,
    /// Emit JSON output.
    #[arg(long = "json")]
    json: bool,
}

#[derive(Args, Debug)]
struct AnswersArgs {
    /// Component reference (oci://, repo://, store://) or local path.
    #[arg(long = "component")]
    component: String,
    /// Component operation (used to select dev_flow graph).
    #[arg(long = "operation")]
    operation: String,
    /// Which dev_flow to use for questions (default uses --operation, config uses "custom").
    #[arg(long = "mode", value_enum, default_value = "default")]
    mode: AnswersMode,
    /// Output file prefix.
    #[arg(long = "name")]
    name: String,
    /// Output directory (defaults to current directory).
    #[arg(long = "out-dir")]
    out_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdateStepArgs {
    /// Component id to resolve via wizard ops (preferred for new flows).
    #[arg(value_name = "component_id")]
    component_id: Option<String>,
    /// Flow file to update.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Node id to update (optional when component metadata exists).
    #[arg(long = "step")]
    step: Option<String>,
    /// Mode: default (default) or config.
    #[arg(long = "mode", default_value = "default", value_parser = ["config", "default"])]
    mode: String,
    /// Optional wizard mode (default/setup/update/remove).
    #[arg(long = "wizard-mode", value_enum)]
    wizard_mode: Option<WizardModeArg>,
    /// Optional new operation name (defaults to existing op key).
    #[arg(long = "operation")]
    operation: Option<String>,
    /// Routing shorthand: make the node terminal (out).
    #[arg(long = "routing-out", conflicts_with_all = ["routing_reply", "routing_next", "routing_multi_to", "routing_json"])]
    routing_out: bool,
    /// Routing shorthand: reply to origin.
    #[arg(long = "routing-reply", conflicts_with_all = ["routing_out", "routing_next", "routing_multi_to", "routing_json"])]
    routing_reply: bool,
    /// Route to a specific node id.
    #[arg(long = "routing-next", conflicts_with_all = ["routing_out", "routing_reply", "routing_multi_to", "routing_json"])]
    routing_next: Option<String>,
    /// Route to multiple node ids (comma-separated).
    #[arg(long = "routing-multi-to", conflicts_with_all = ["routing_out", "routing_reply", "routing_next", "routing_json"])]
    routing_multi_to: Option<String>,
    /// Explicit routing JSON file (escape hatch).
    #[arg(long = "routing-json", conflicts_with_all = ["routing_out", "routing_reply", "routing_next", "routing_multi_to"])]
    routing_json: Option<PathBuf>,
    /// Answers JSON/YAML string to merge with existing payload.
    #[arg(long = "answers")]
    answers: Option<String>,
    /// Answers file (JSON/YAML) to merge with existing payload.
    #[arg(long = "answers-file")]
    answers_file: Option<PathBuf>,
    /// Directory for wizard answers artifacts.
    #[arg(long = "answers-dir")]
    answers_dir: Option<PathBuf>,
    /// Overwrite existing answers artifacts.
    #[arg(long = "overwrite-answers")]
    overwrite_answers: bool,
    /// Force re-asking wizard questions even if answers exist.
    #[arg(long = "reask")]
    reask: bool,
    /// Locale (BCP47) for wizard prompts.
    #[arg(long = "locale")]
    locale: Option<String>,
    /// Non-interactive mode (merge answers/prefill; fail if required missing).
    #[arg(long = "non-interactive")]
    non_interactive: bool,
    /// Allow interactive QA prompts (wizard mode only).
    #[arg(long = "interactive")]
    interactive: bool,
    /// Optional component reference (oci://, repo://, store://).
    #[arg(long = "component")]
    component: Option<String>,
    /// Local wasm path for wizard ops (relative to the flow file).
    #[arg(long = "local-wasm")]
    local_wasm: Option<PathBuf>,
    /// Distributor URL for component-id resolution.
    #[arg(long = "distributor-url")]
    distributor_url: Option<String>,
    /// Distributor auth token (optional).
    #[arg(long = "auth-token")]
    auth_token: Option<String>,
    /// Tenant id for component-id resolution.
    #[arg(long = "tenant")]
    tenant: Option<String>,
    /// Environment id for component-id resolution.
    #[arg(long = "env")]
    env: Option<String>,
    /// Pack id for component-id resolution.
    #[arg(long = "pack")]
    pack: Option<String>,
    /// Component version for component-id resolution.
    #[arg(long = "component-version")]
    component_version: Option<String>,
    /// ABI version override for wizard ops.
    #[arg(long = "abi-version")]
    abi_version: Option<String>,
    /// Resolver override (fixture://...) for tests/CI.
    #[arg(long = "resolver")]
    resolver: Option<String>,
    /// Show the updated flow without writing it.
    #[arg(long = "dry-run")]
    dry_run: bool,
    /// Backward-compatible write flag (ignored; writing is default).
    #[arg(long = "write", hide = true)]
    write: bool,
    /// Allow contract drift when describe_hash changes.
    #[arg(long = "allow-contract-change")]
    allow_contract_change: bool,
}

#[derive(Args, Debug, Clone)]
struct DeleteStepArgs {
    /// Component id to resolve via wizard ops (preferred for new flows).
    #[arg(value_name = "component_id")]
    component_id: Option<String>,
    /// Flow file to update.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Node id to delete (optional when component metadata exists).
    #[arg(long = "step")]
    step: Option<String>,
    /// Optional wizard mode (default/setup/update/remove).
    #[arg(long = "wizard-mode", value_enum)]
    wizard_mode: Option<WizardModeArg>,
    /// Answers JSON/YAML string to merge with wizard prompts.
    #[arg(long = "answers")]
    answers: Option<String>,
    /// Answers file (JSON/YAML).
    #[arg(long = "answers-file")]
    answers_file: Option<PathBuf>,
    /// Directory for wizard answers artifacts.
    #[arg(long = "answers-dir")]
    answers_dir: Option<PathBuf>,
    /// Overwrite existing answers artifacts.
    #[arg(long = "overwrite-answers")]
    overwrite_answers: bool,
    /// Force re-asking wizard questions even if answers exist.
    #[arg(long = "reask")]
    reask: bool,
    /// Locale (BCP47) for wizard prompts.
    #[arg(long = "locale")]
    locale: Option<String>,
    /// Allow interactive QA prompts (wizard mode only).
    #[arg(long = "interactive")]
    interactive: bool,
    /// Optional component reference (oci://, repo://, store://).
    #[arg(long = "component")]
    component: Option<String>,
    /// Local wasm path for wizard ops (relative to the flow file).
    #[arg(long = "local-wasm")]
    local_wasm: Option<PathBuf>,
    /// Distributor URL for component-id resolution.
    #[arg(long = "distributor-url")]
    distributor_url: Option<String>,
    /// Distributor auth token (optional).
    #[arg(long = "auth-token")]
    auth_token: Option<String>,
    /// Tenant id for component-id resolution.
    #[arg(long = "tenant")]
    tenant: Option<String>,
    /// Environment id for component-id resolution.
    #[arg(long = "env")]
    env: Option<String>,
    /// Pack id for component-id resolution.
    #[arg(long = "pack")]
    pack: Option<String>,
    /// Component version for component-id resolution.
    #[arg(long = "component-version")]
    component_version: Option<String>,
    /// ABI version override for wizard ops.
    #[arg(long = "abi-version")]
    abi_version: Option<String>,
    /// Resolver override (fixture://...) for tests/CI.
    #[arg(long = "resolver")]
    resolver: Option<String>,
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

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum AnswersMode {
    Default,
    Config,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let schema_mode = SchemaMode::resolve(cli.permissive)?;
    match cli.command {
        Commands::New(args) => handle_new(args, cli.backup),
        Commands::Update(args) => handle_update(args, cli.backup),
        Commands::AddStep(args) => handle_add_step(args, schema_mode, cli.format, cli.backup),
        Commands::UpdateStep(args) => handle_update_step(args, schema_mode, cli.format, cli.backup),
        Commands::DeleteStep(args) => handle_delete_step(args, cli.format, cli.backup),
        Commands::Doctor(mut args) => {
            if matches!(cli.format, OutputFormat::Json) {
                args.json = true;
            }
            handle_doctor(args, schema_mode)
        }
        Commands::DoctorAnswers(args) => handle_doctor_answers(args),
        Commands::Answers(args) => handle_answers(args, schema_mode),
        Commands::BindComponent(args) => handle_bind_component(args),
        Commands::Wizard(args) => handle_wizard(args, schema_mode, cli.format, cli.backup),
    }
}

fn handle_wizard(
    args: WizardArgs,
    schema_mode: SchemaMode,
    format: OutputFormat,
    backup: bool,
) -> Result<()> {
    match args.command {
        WizardCommand::New(args) => handle_new(args, backup),
        WizardCommand::Edit(args) => handle_update(args, backup),
        WizardCommand::AddStep(mut args) => {
            args.args.interactive = !args.non_interactive;
            if args.args.wizard_mode.is_none() {
                args.args.wizard_mode = Some(WizardModeArg::Default);
            }
            handle_add_step(args.args, schema_mode, format, backup)
        }
        WizardCommand::UpdateStep(mut args) => {
            args.args.interactive = !args.non_interactive;
            if args.args.wizard_mode.is_none() {
                args.args.wizard_mode = Some(WizardModeArg::Update);
            }
            handle_update_step(args.args, schema_mode, format, backup)
        }
        WizardCommand::RemoveStep(mut args) => {
            args.args.interactive = !args.non_interactive;
            if args.args.wizard_mode.is_none() {
                args.args.wizard_mode = Some(WizardModeArg::Remove);
            }
            handle_delete_step(args.args, format, backup)
        }
    }
}

fn handle_doctor(args: DoctorArgs, schema_mode: SchemaMode) -> Result<()> {
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
    let lint_ctx = LintContext {
        schema_text: &schema_text,
        schema_label: &schema_label,
        schema_path: schema_path.as_path(),
        registry: registry.as_ref(),
        schema_mode,
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
            schema_mode,
        );
    }

    let mut failures = 0usize;
    for target in &args.targets {
        lint_path(target, &lint_ctx, true, &mut failures)?;
        if target.is_file() {
            let mut contract_diags = validate_contracts_for_flow(target, args.online)?;
            contract_diags.sort_by(|a, b| {
                a.node_id
                    .cmp(&b.node_id)
                    .then_with(|| a.severity.cmp(&b.severity))
                    .then_with(|| a.code.cmp(b.code))
            });
            for diag in &contract_diags {
                match diag.severity {
                    ContractSeverity::Error => {
                        eprintln!("error: {} ({}:{})", diag.message, diag.node_id, diag.code)
                    }
                    ContractSeverity::Warning => {
                        eprintln!("warning: {} ({}:{})", diag.message, diag.node_id, diag.code)
                    }
                }
            }
            if contract_diags
                .iter()
                .any(|d| matches!(d.severity, ContractSeverity::Error))
            {
                failures += 1;
            }
        }
    }

    if failures == 0 {
        println!("All flows valid");
        Ok(())
    } else {
        Err(anyhow::anyhow!("{failures} flow(s) failed validation"))
    }
}

fn handle_new(args: NewArgs, backup: bool) -> Result<()> {
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
        meta: None,
        nodes: IndexMap::new(),
    };
    let mut yaml = serde_yaml_bw::to_string(&doc)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    write_flow_file(&args.flow_path, &yaml, args.force, backup)?;
    println!(
        "Created flow '{}' at {} (type: {})",
        args.flow_id,
        args.flow_path.display(),
        args.flow_type
    );
    Ok(())
}

fn handle_doctor_answers(args: DoctorAnswersArgs) -> Result<()> {
    let schema_text = fs::read_to_string(&args.schema)
        .with_context(|| format!("read schema {}", args.schema.display()))?;
    let answers_text = fs::read_to_string(&args.answers)
        .with_context(|| format!("read answers {}", args.answers.display()))?;
    let schema: serde_json::Value =
        serde_json::from_str(&schema_text).context("parse schema as JSON")?;
    let answers: serde_json::Value =
        serde_json::from_str(&answers_text).context("parse answers as JSON")?;

    let compiled = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .context("compile answers schema")?;
    if let Err(error) = compiled.validate(&answers) {
        let messages = vec![error.to_string()];
        if args.json {
            let payload = json!({ "ok": false, "errors": messages });
            print_json_payload(&payload)?;
            std::process::exit(1);
        } else {
            for msg in &messages {
                eprintln!("error: {msg}");
            }
        }
        anyhow::bail!("answers failed schema validation");
    }

    if args.json {
        let payload = json!({ "ok": true, "errors": [] });
        print_json_payload(&payload)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum ContractSeverity {
    Error,
    Warning,
}

struct ContractDiagnostic {
    code: &'static str,
    severity: ContractSeverity,
    message: String,
    node_id: String,
}

fn validate_contracts_for_flow(flow_path: &Path, online: bool) -> Result<Vec<ContractDiagnostic>> {
    let doc = load_ygtc_from_path(flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    let mut diags = Vec::new();

    for (node_id, node) in &flow_ir.nodes {
        if !node_payload_looks_like_component(&node.payload) {
            continue;
        }
        let meta = flow_ir
            .meta
            .as_ref()
            .and_then(|meta| meta.as_object())
            .and_then(|root| root.get(flow_meta::META_NAMESPACE))
            .and_then(|value| value.as_object())
            .and_then(|greentic| greentic.get("components"))
            .and_then(|value| value.as_object())
            .and_then(|components| components.get(node_id))
            .and_then(|value| value.as_object());

        let Some(entry) = meta else {
            diags.push(ContractDiagnostic {
                code: "FLOW_MISSING_METADATA",
                severity: ContractSeverity::Error,
                message: "missing component contract metadata".to_string(),
                node_id: node_id.clone(),
            });
            continue;
        };

        for field in ["describe_hash", "schema_hash", "operation_id"] {
            if entry.get(field).and_then(|v| v.as_str()).is_none() {
                diags.push(ContractDiagnostic {
                    code: "FLOW_MISSING_METADATA",
                    severity: ContractSeverity::Error,
                    message: format!("missing required metadata field '{field}'"),
                    node_id: node_id.clone(),
                });
            }
        }

        if !online {
            if let Some(schema_hex) = entry.get("config_schema_cbor").and_then(|v| v.as_str()) {
                let schema_bytes = match hex_to_bytes(schema_hex) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        diags.push(ContractDiagnostic {
                            code: "FLOW_SCHEMA_DECODE",
                            severity: ContractSeverity::Error,
                            message: format!("failed to decode stored config schema: {err}"),
                            node_id: node_id.clone(),
                        });
                        continue;
                    }
                };
                let schema: greentic_types::schemas::common::schema_ir::SchemaIr =
                    match greentic_types::cbor::canonical::from_cbor(&schema_bytes) {
                        Ok(schema) => schema,
                        Err(err) => {
                            diags.push(ContractDiagnostic {
                                code: "FLOW_SCHEMA_DECODE",
                                severity: ContractSeverity::Error,
                                message: format!("failed to parse stored config schema: {err}"),
                                node_id: node_id.clone(),
                            });
                            continue;
                        }
                    };
                let config_value = extract_config_value(&node.payload);
                let config_cbor =
                    greentic_types::cbor::canonical::to_canonical_cbor_allow_floats(&config_value)
                        .map_err(|err| anyhow!("encode config for validation: {err}"))?;
                let config_val: ciborium::value::Value =
                    ciborium::de::from_reader(config_cbor.as_slice())
                        .map_err(|err| anyhow!("decode config cbor: {err}"))?;
                let schema_diags = validate_value_against_schema(&schema, &config_val);
                for diag in schema_diags {
                    let severity = match diag.severity {
                        Severity::Error => ContractSeverity::Error,
                        Severity::Warning => ContractSeverity::Warning,
                    };
                    diags.push(ContractDiagnostic {
                        code: diag.code,
                        severity,
                        message: diag.message,
                        node_id: node_id.clone(),
                    });
                }
            } else {
                diags.push(ContractDiagnostic {
                    code: "FLOW_SCHEMA_MISSING",
                    severity: ContractSeverity::Warning,
                    message: "missing stored config schema for offline validation".to_string(),
                    node_id: node_id.clone(),
                });
            }
            continue;
        }

        let Some(operation_id) = entry
            .get("operation_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        else {
            continue;
        };

        let Some(sidecar) = read_flow_resolve(flow_path).ok() else {
            diags.push(ContractDiagnostic {
                code: "FLOW_MISSING_SIDECAR",
                severity: ContractSeverity::Error,
                message: "missing resolve sidecar for online validation".to_string(),
                node_id: node_id.clone(),
            });
            continue;
        };
        let Some(node_resolve) = sidecar.nodes.get(node_id) else {
            diags.push(ContractDiagnostic {
                code: "FLOW_MISSING_SIDECAR",
                severity: ContractSeverity::Error,
                message: "missing sidecar entry for node".to_string(),
                node_id: node_id.clone(),
            });
            continue;
        };

        let resolved = resolve_source_to_wasm(flow_path, &node_resolve.source)?;
        let spec = wizard_ops::fetch_wizard_spec(&resolved, wizard_ops::WizardMode::Default)?;
        let (describe, computed_meta) = derive_contract_meta(&spec.describe_cbor, &operation_id)?;

        if let Some(stored) = entry.get("describe_hash").and_then(|v| v.as_str())
            && stored != computed_meta.describe_hash
        {
            diags.push(ContractDiagnostic {
                code: "FLOW_CONTRACT_DRIFT",
                severity: ContractSeverity::Error,
                message: "describe_hash mismatch (contract drift)".to_string(),
                node_id: node_id.clone(),
            });
        }
        if let Some(stored) = entry.get("schema_hash").and_then(|v| v.as_str())
            && stored != computed_meta.schema_hash
        {
            diags.push(ContractDiagnostic {
                code: "FLOW_SCHEMA_HASH_MISMATCH",
                severity: ContractSeverity::Error,
                message: "schema_hash mismatch".to_string(),
                node_id: node_id.clone(),
            });
        }

        let config_value = extract_config_value(&node.payload);
        let config_cbor =
            greentic_types::cbor::canonical::to_canonical_cbor_allow_floats(&config_value)
                .map_err(|err| anyhow!("encode config for validation: {err}"))?;
        let schema_diags = validate_value_against_schema(
            &describe.config_schema,
            &ciborium::de::from_reader(config_cbor.as_slice())
                .map_err(|err| anyhow!("decode config cbor: {err}"))?,
        );
        for diag in schema_diags {
            let severity = match diag.severity {
                Severity::Error => ContractSeverity::Error,
                Severity::Warning => ContractSeverity::Warning,
            };
            diags.push(ContractDiagnostic {
                code: diag.code,
                severity,
                message: diag.message,
                node_id: node_id.clone(),
            });
        }
    }

    Ok(diags)
}

fn existing_describe_hash(meta: &Option<serde_json::Value>, node_id: &str) -> Option<String> {
    meta.as_ref()
        .and_then(|root| root.as_object())
        .and_then(|root| root.get(flow_meta::META_NAMESPACE))
        .and_then(|value| value.as_object())
        .and_then(|greentic| greentic.get("components"))
        .and_then(|value| value.as_object())
        .and_then(|components| components.get(node_id))
        .and_then(|value| value.as_object())
        .and_then(|entry| entry.get("describe_hash"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn node_payload_looks_like_component(payload: &serde_json::Value) -> bool {
    if let Some(obj) = payload.as_object() {
        if obj.contains_key("component") || obj.contains_key("config") {
            return true;
        }
        if let Some(exec) = obj.get("component.exec") {
            return exec.is_object();
        }
    }
    false
}

fn extract_config_value(payload: &serde_json::Value) -> serde_json::Value {
    if let Some(obj) = payload.as_object()
        && let Some(config) = obj.get("config")
    {
        return config.clone();
    }
    payload.clone()
}

fn resolve_source_to_wasm(flow_path: &Path, source: &ComponentSourceRefV1) -> Result<Vec<u8>> {
    match source {
        ComponentSourceRefV1::Local { path, .. } => {
            let local_path = local_path_from_sidecar(path, flow_path);
            let bytes = fs::read(&local_path)
                .with_context(|| format!("read wasm at {}", local_path.display()))?;
            Ok(bytes)
        }
        ComponentSourceRefV1::Oci { r#ref, .. }
        | ComponentSourceRefV1::Repo { r#ref, .. }
        | ComponentSourceRefV1::Store { r#ref, .. } => {
            let resolved = resolve_ref_to_bytes(r#ref, None)?;
            Ok(resolved.bytes)
        }
    }
}

fn handle_answers(args: AnswersArgs, schema_mode: SchemaMode) -> Result<()> {
    let manifest_path = resolve_manifest_path_for_component(&args.component)?;
    let manifest = load_manifest_json(&manifest_path)?;
    let requested_flow = match args.mode {
        AnswersMode::Default => args.operation.as_str(),
        AnswersMode::Config => "custom",
    };
    let (questions, used_flow) = questions_for_operation(&manifest, requested_flow)?;
    if used_flow.as_deref() != Some(requested_flow)
        && let Some(flow) = &used_flow
    {
        eprintln!(
            "warning: dev_flows.{} not found; using dev_flows.{} for questions",
            requested_flow, flow
        );
    }

    let flow_name = used_flow.as_deref().unwrap_or(requested_flow);
    let source_desc = format!("dev_flows.{flow_name}");
    let component_id = manifest
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let schema = schema_for_questions(&questions);
    let use_manifest_schema = questions.is_empty() || is_effectively_empty_schema(&schema);
    let schema_resolution = if use_manifest_schema {
        Some(resolve_input_schema(&manifest_path, &args.operation)?)
    } else {
        None
    };
    let (schema_source_desc, schema_operation, schema_manifest_path, schema_component_id) =
        if let Some(resolution) = &schema_resolution {
            (
                "operations[].input_schema".to_string(),
                resolution.operation.clone(),
                resolution.manifest_path.as_path(),
                resolution.component_id.as_str(),
            )
        } else {
            (
                source_desc,
                flow_name.to_string(),
                manifest_path.as_path(),
                component_id.as_str(),
            )
        };
    let schema_ref = if let Some(resolution) = &schema_resolution {
        resolution.schema.as_ref()
    } else {
        Some(&schema)
    };
    require_schema(
        schema_mode,
        schema_component_id,
        &schema_operation,
        schema_manifest_path,
        &schema_source_desc,
        schema_ref,
    )?;

    let example = example_for_questions(&questions);
    validate_example_against_schema(&schema, &example)?;

    let out_dir = match args.out_dir {
        Some(dir) => dir,
        None => env::current_dir().context("resolve current directory")?,
    };
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("create output dir {}", out_dir.display()))?;
    let schema_path = out_dir.join(format!("{}.schema.json", args.name));
    let example_path = out_dir.join(format!("{}.example.json", args.name));
    write_json_file(&schema_path, &schema)?;
    write_json_file(&example_path, &example)?;
    println!(
        "Wrote answers schema to {} and example to {}",
        schema_path.display(),
        example_path.display()
    );
    Ok(())
}

fn handle_update(args: UpdateArgs, backup: bool) -> Result<()> {
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
    write_flow_file(&args.flow_path, &yaml, true, backup)?;
    println!("Updated flow metadata at {}", args.flow_path.display());
    Ok(())
}

struct LintContext<'a> {
    schema_text: &'a str,
    schema_label: &'a str,
    schema_path: &'a Path,
    registry: Option<&'a AdapterCatalog>,
    schema_mode: SchemaMode,
}

fn lint_path(
    path: &Path,
    ctx: &LintContext<'_>,
    interactive: bool,
    failures: &mut usize,
) -> Result<()> {
    if path.is_file() {
        lint_file(path, ctx, interactive, failures)?;
    } else if path.is_dir() {
        let entries = fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?;
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read directory entry in {}", path.display()))?;
            lint_path(&entry.path(), ctx, interactive, failures)?;
        }
    }
    Ok(())
}

fn lint_file(
    path: &Path,
    ctx: &LintContext<'_>,
    interactive: bool,
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
        ctx.schema_text,
        ctx.schema_label,
        ctx.schema_path,
        ctx.registry,
        ctx.schema_mode,
    ) {
        Ok(result) => {
            let mut had_errors = false;
            if result.lint_errors.is_empty() {
                if result.bundle.kind != "component-config" {
                    let validation =
                        validate_sidecar_for_flow(path, &result.flow, interactive, true)?;
                    let mut sidecar_error = false;
                    if !validation.missing.is_empty() {
                        eprintln!(
                            "ERR  {}: missing sidecar entries for nodes: {}",
                            path.display(),
                            validation.missing.join(", ")
                        );
                        sidecar_error = true;
                    }
                    if !validation.extra.is_empty() {
                        eprintln!(
                            "ERR  {}: unused sidecar entries: {}",
                            path.display(),
                            validation.extra.join(", ")
                        );
                        sidecar_error = true;
                    }
                    if !validation.invalid.is_empty() {
                        eprintln!(
                            "ERR  {}: invalid sidecar entries: {}",
                            path.display(),
                            validation.invalid.join(", ")
                        );
                        sidecar_error = true;
                    }
                    if sidecar_error {
                        *failures += 1;
                        had_errors = true;
                    }
                    if validation.updated {
                        println!("Updated sidecar {}", validation.path.display());
                    }
                }
                if !had_errors {
                    println!("OK  {} ({})", path.display(), result.bundle.id);
                }
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
    flow: greentic_types::Flow,
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
    schema_mode: SchemaMode,
) -> Result<LintResult, FlowError> {
    let (bundle, flow) = load_and_validate_bundle_with_schema_text(
        content,
        schema_text,
        schema_label.to_string(),
        Some(schema_path),
        source_path,
    )?;
    let mut lint_errors = if let Some(cat) = registry {
        lint_with_registry(&flow, cat)
    } else {
        lint_builtin_rules(&flow)
    };
    lint_errors.extend(lint_component_configs(
        &flow,
        source_path,
        bundle.kind.as_str(),
        schema_mode,
    ));
    Ok(LintResult {
        bundle,
        flow,
        lint_errors,
    })
}

fn lint_component_configs(
    flow: &greentic_types::Flow,
    source_path: Option<&Path>,
    flow_kind: &str,
    schema_mode: SchemaMode,
) -> Vec<String> {
    if flow_kind == "component-config" {
        return Vec::new();
    }
    let Some(flow_path) = source_path else {
        return Vec::new();
    };
    if !flow_path.exists() {
        return Vec::new();
    }
    let sidecar_path = sidecar_path_for_flow(flow_path);
    if !sidecar_path.exists() {
        return Vec::new();
    }
    let sidecar = match read_flow_resolve(&sidecar_path) {
        Ok(doc) => doc,
        Err(err) => {
            return vec![format!(
                "component_config: failed to read sidecar {}: {err}",
                sidecar_path.display()
            )];
        }
    };

    let mut errors = Vec::new();
    for (node_id, node) in &flow.nodes {
        let node_key = node_id.as_str();
        if matches!(node.component.id.as_str(), "questions" | "template") {
            continue;
        }
        let Some(entry) = sidecar.nodes.get(node_key) else {
            continue;
        };
        let manifest_path = match resolve_component_manifest_path(&entry.source, flow_path) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let operation = node.component.operation.as_deref().unwrap_or("unknown");
        let schema_resolution = match resolve_input_schema(&manifest_path, operation) {
            Ok(resolution) => resolution,
            Err(err) => {
                errors.push(format!(
                    "component_config: node '{node_key}' failed to read {}: {err}",
                    manifest_path.display()
                ));
                continue;
            }
        };
        let source_desc = "operations[].input_schema";
        let schema_ref = match require_schema(
            schema_mode,
            &schema_resolution.component_id,
            &schema_resolution.operation,
            &schema_resolution.manifest_path,
            source_desc,
            schema_resolution.schema.as_ref(),
        ) {
            Ok(Some(schema)) => schema,
            Ok(None) => continue,
            Err(err) => {
                errors.push(err.to_string());
                continue;
            }
        };
        let validator = match jsonschema_options_with_base(Some(manifest_path.as_path()))
            .build(schema_ref)
        {
            Ok(validator) => validator,
            Err(err) => {
                if let ValidationErrorKind::Referencing(ReferencingError::Unretrievable {
                    uri, ..
                }) = err.kind()
                    && uri.starts_with("file://")
                    && !Path::new(uri.trim_start_matches("file://")).exists()
                {
                    eprintln!(
                        "WARN component_config: node '{node_key}' schema validation for component '{}' skipped because '{uri}' is missing (manifest: {}). Continuing without this schema.",
                        schema_resolution.component_id,
                        manifest_path.display()
                    );
                    continue;
                }
                errors.push(format!(
                    "component_config: node '{node_key}' schema compile failed for component '{}': {err}",
                    schema_resolution.component_id
                ));
                continue;
            }
        };
        let payload = match resolve_parameters(
            &node.input.mapping,
            &flow.metadata.extra,
            &format!("nodes.{node_key}"),
        ) {
            Ok(value) => value,
            Err(err) => {
                errors.push(format!(
                    "component_config: node '{node_key}' parameters resolution failed: {err}",
                ));
                continue;
            }
        };
        for err in validator.iter_errors(&payload) {
            let pointer = err.instance_path().to_string();
            let pointer = if pointer.is_empty() {
                "/".to_string()
            } else {
                pointer
            };
            errors.push(format!(
                "component_config: node '{node_key}' payload invalid for component '{}' at {pointer}: {err}",
                schema_resolution.component_id
            ));
        }
    }

    errors
}

fn run_json(
    targets: &[PathBuf],
    stdin_content: Option<String>,
    schema_text: &str,
    schema_label: &str,
    schema_path: &Path,
    registry: Option<&AdapterCatalog>,
    schema_mode: SchemaMode,
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

    let lint_result = lint_flow(
        &content,
        source_path,
        schema_text,
        schema_label,
        schema_path,
        registry,
        schema_mode,
    );

    let output = match lint_result {
        Ok(result) => {
            if !result.lint_errors.is_empty() {
                LintJsonOutput::lint_failure(result.lint_errors, Some(source_display.clone()))
            } else if let Some(path) = source_path
                && path.exists()
            {
                if result.bundle.kind == "component-config" {
                    LintJsonOutput::success(result.bundle)
                } else {
                    let validation = validate_sidecar_for_flow(path, &result.flow, false, false)?;
                    let mut errors = Vec::new();
                    if !validation.missing.is_empty() {
                        errors.push(format!(
                            "missing sidecar entries for nodes: {}",
                            validation.missing.join(", ")
                        ));
                    }
                    if !validation.extra.is_empty() {
                        errors.push(format!(
                            "unused sidecar entries: {}",
                            validation.extra.join(", ")
                        ));
                    }
                    if !validation.invalid.is_empty() {
                        errors.push(format!(
                            "invalid sidecar entries: {}",
                            validation.invalid.join(", ")
                        ));
                    }
                    if errors.is_empty() {
                        LintJsonOutput::success(result.bundle)
                    } else {
                        LintJsonOutput::lint_failure(
                            errors,
                            Some(validation.path.display().to_string()),
                        )
                    }
                }
            } else {
                LintJsonOutput::success(result.bundle)
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

fn confirm_delete_unused(path: &Path, unused: &[String]) -> Result<bool> {
    eprintln!(
        "Unused sidecar entries detected in {}: {}",
        path.display(),
        unused.join(", ")
    );
    eprint!("Delete unused sidecar entries? [y/N]: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return Ok(false);
    }
    let response = input.trim().to_lowercase();
    Ok(response == "y" || response == "yes")
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
    use super::AddStepArgs;
    use super::AddStepMode;
    use super::DeleteStepArgs;
    use super::NewArgs;
    use super::OutputFormat;
    use super::SchemaMode;
    use super::UpdateStepArgs;
    use super::WizardModeArg;
    use super::handle_add_step;
    use super::handle_delete_step;
    use super::handle_new;
    use super::handle_update_step;
    use super::parse_answers_map;
    use super::resolve_config_flow;
    use super::serialize_doc;
    use greentic_flow::flow_ir::FlowIr;
    use greentic_flow::loader::load_ygtc_from_path;
    use greentic_flow::wizard_ops::WizardMode;
    use serde_json::Value;
    use serde_json::json;
    use std::env;
    use std::fs;
    use tempfile::NamedTempFile;
    use tempfile::tempdir;

    fn extract_config_payload(payload: &serde_json::Value) -> &serde_json::Value {
        payload
            .get("config")
            .and_then(|value| value.as_object().map(|_| value))
            .unwrap_or(payload)
    }

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
            resolve_config_flow(None, &[manifest_file.path().to_path_buf()], "default")
                .expect("resolve");
        assert!(yaml.contains("id: cfg"));
        assert!(
            schema_path.starts_with(env::temp_dir()),
            "expected schema path {schema_path:?} under the temp directory"
        );
    }

    #[test]
    fn config_flow_schema_resides_in_temp_dir() {
        let manifest = json!({
            "id": "ai.greentic.custom",
            "dev_flows": {
                "custom": {
                    "graph": {
                        "id": "cfg",
                        "type": "component-config",
                        "nodes": {}
                    }
                }
            }
        });
        let manifest_file = NamedTempFile::new().expect("temp file");
        fs::write(manifest_file.path(), manifest.to_string()).expect("write manifest");

        let (_, schema_path) =
            resolve_config_flow(None, &[manifest_file.path().to_path_buf()], "custom")
                .expect("resolve");
        assert!(
            schema_path.starts_with(env::temp_dir()),
            "expected schema path {schema_path:?} to live in temp dir"
        );
    }

    #[test]
    fn answers_merge_prefers_cli_over_file() {
        let file = NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), r#"{"value":"from-file","keep":1}"#).unwrap();
        let merged = parse_answers_map(Some(r#"{"value":"from-cli"}"#), Some(file.path())).unwrap();
        assert_eq!(
            merged.get("value").and_then(|v| v.as_str()),
            Some("from-cli")
        );
        assert_eq!(merged.get("keep").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn answers_map_accepts_yaml() {
        let merged = parse_answers_map(Some("value: hello\ncount: 2"), None).unwrap();
        assert_eq!(merged.get("value").and_then(|v| v.as_str()), Some("hello"));
        assert_eq!(merged.get("count").and_then(|v| v.as_i64()), Some(2));
    }

    fn fixture_registry_resolver() -> String {
        let registry = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("registry");
        format!("fixture://{}", registry.display())
    }

    #[test]
    fn fixture_registry_resolves_wizard() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flow.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "main".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("create flow");

        let resolver = fixture_registry_resolver();

        let args = AddStepArgs {
            component_id: None,
            flow_path: flow_path.clone(),
            after: None,
            mode: AddStepMode::Default,
            pack_alias: None,
            wizard_mode: Some(WizardModeArg::Default),
            operation: None,
            payload: "{}".to_string(),
            routing_out: true,
            routing_reply: false,
            routing_next: None,
            routing_multi_to: None,
            routing_json: None,
            routing_to_anchor: false,
            config_flow: None,
            answers: None,
            answers_file: None,
            answers_dir: None,
            overwrite_answers: false,
            reask: false,
            locale: None,
            interactive: false,
            allow_cycles: false,
            dry_run: false,
            write: false,
            validate_only: false,
            manifests: Vec::new(),
            node_id: Some("widget".to_string()),
            component_ref: Some("oci://acme/widget:1".to_string()),
            local_wasm: None,
            distributor_url: None,
            auth_token: None,
            tenant: None,
            env: None,
            pack: None,
            component_version: None,
            abi_version: None,
            resolver: Some(resolver),
            pin: false,
            allow_contract_change: false,
        };
        handle_add_step(args, SchemaMode::Strict, OutputFormat::Human, false).expect("add step");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        let node = flow_ir.nodes.get("widget").expect("node exists");
        assert_eq!(node.operation, "run");
        let config = extract_config_payload(&node.payload);
        assert_eq!(config.get("foo").and_then(|v| v.as_str()), Some("bar"));
    }

    #[test]
    fn wizard_mode_upgrade_alias_maps_to_update() {
        assert_eq!(
            WizardModeArg::UpgradeDeprecated.to_mode(),
            WizardMode::Update
        );
    }

    #[test]
    fn fixture_registry_update_and_remove() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flow.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "main".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("create flow");

        let resolver = fixture_registry_resolver();
        handle_add_step(
            AddStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                after: None,
                mode: AddStepMode::Default,
                pack_alias: None,
                wizard_mode: Some(WizardModeArg::Default),
                operation: None,
                payload: "{}".to_string(),
                routing_out: true,
                routing_reply: false,
                routing_next: None,
                routing_multi_to: None,
                routing_json: None,
                routing_to_anchor: false,
                config_flow: None,
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                interactive: false,
                allow_cycles: false,
                dry_run: false,
                write: false,
                validate_only: false,
                manifests: Vec::new(),
                node_id: Some("widget".to_string()),
                component_ref: Some("oci://acme/widget:1".to_string()),
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: Some(resolver.clone()),
                pin: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        )
        .expect("add step");

        handle_update_step(
            UpdateStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                step: Some("widget".to_string()),
                mode: "default".to_string(),
                wizard_mode: Some(WizardModeArg::Update),
                operation: None,
                routing_out: false,
                routing_reply: false,
                routing_next: None,
                routing_multi_to: None,
                routing_json: None,
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                non_interactive: true,
                interactive: false,
                component: Some("oci://acme/widget:1".to_string()),
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: Some(resolver.clone()),
                dry_run: false,
                write: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        )
        .expect("update step");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        let node = flow_ir.nodes.get("widget").expect("node exists");
        let config = extract_config_payload(&node.payload);
        assert_eq!(config.get("foo").and_then(|v| v.as_str()), Some("updated"));

        handle_delete_step(
            DeleteStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                step: Some("widget".to_string()),
                wizard_mode: Some(WizardModeArg::Remove),
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                interactive: false,
                component: Some("oci://acme/widget:1".to_string()),
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: Some(resolver),
                strategy: "splice".to_string(),
                multi_pred: "error".to_string(),
                assume_yes: true,
                write: true,
            },
            OutputFormat::Human,
            false,
        )
        .expect("delete step");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(flow_ir.nodes.is_empty());
    }

    #[test]
    fn update_step_blocks_contract_drift() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flow.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "main".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("create flow");

        let resolver = fixture_registry_resolver();
        handle_add_step(
            AddStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                after: None,
                mode: AddStepMode::Default,
                pack_alias: None,
                wizard_mode: Some(WizardModeArg::Default),
                operation: None,
                payload: "{}".to_string(),
                routing_out: true,
                routing_reply: false,
                routing_next: None,
                routing_multi_to: None,
                routing_json: None,
                routing_to_anchor: false,
                config_flow: None,
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                interactive: false,
                allow_cycles: false,
                dry_run: false,
                write: false,
                validate_only: false,
                manifests: Vec::new(),
                node_id: Some("widget".to_string()),
                component_ref: Some("oci://acme/widget:1".to_string()),
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: Some(resolver.clone()),
                pin: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        )
        .expect("add step");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let mut flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        if let Some(meta) = flow_ir.meta.as_mut()
            && let Some(root) = meta.as_object_mut()
            && let Some(greentic) = root.get_mut("greentic").and_then(Value::as_object_mut)
            && let Some(components) = greentic
                .get_mut("components")
                .and_then(Value::as_object_mut)
            && let Some(entry) = components.get_mut("widget").and_then(Value::as_object_mut)
        {
            entry.insert(
                "describe_hash".to_string(),
                Value::String("deadbeef".to_string()),
            );
        }
        let doc_out = flow_ir.to_doc().expect("to doc");
        let yaml = serialize_doc(&doc_out).expect("serialize doc");
        fs::write(&flow_path, yaml).expect("write flow");

        let result = handle_update_step(
            UpdateStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                step: Some("widget".to_string()),
                mode: "default".to_string(),
                wizard_mode: Some(WizardModeArg::Update),
                operation: None,
                routing_out: false,
                routing_reply: false,
                routing_next: None,
                routing_multi_to: None,
                routing_json: None,
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                non_interactive: true,
                interactive: false,
                component: Some("oci://acme/widget:1".to_string()),
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: Some(resolver),
                dry_run: false,
                write: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        );
        assert!(result.is_err(), "expected drift to fail");
    }

    #[test]
    fn add_step_dry_run_does_not_write() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flow.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "main".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("create flow");

        let resolver = fixture_registry_resolver();
        handle_add_step(
            AddStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                after: None,
                mode: AddStepMode::Default,
                pack_alias: None,
                wizard_mode: Some(WizardModeArg::Default),
                operation: None,
                payload: "{}".to_string(),
                routing_out: true,
                routing_reply: false,
                routing_next: None,
                routing_multi_to: None,
                routing_json: None,
                routing_to_anchor: false,
                config_flow: None,
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                interactive: false,
                allow_cycles: false,
                dry_run: true,
                write: false,
                validate_only: false,
                manifests: Vec::new(),
                node_id: Some("widget".to_string()),
                component_ref: Some("oci://acme/widget:1".to_string()),
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: Some(resolver),
                pin: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        )
        .expect("dry run");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(flow_ir.nodes.is_empty(), "dry-run should not write flow");
    }
}
fn backup_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".bak");
    PathBuf::from(os)
}

fn write_flow_file(path: &Path, content: &str, force: bool, backup: bool) -> Result<()> {
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

    if backup && path.exists() {
        let bak = backup_path(path);
        fs::copy(path, &bak)
            .with_context(|| format!("failed to write backup {}", bak.display()))?;
    }
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn resolve_config_flow(
    config_flow_arg: Option<PathBuf>,
    manifests: &[PathBuf],
    flow_name: &str,
) -> Result<(String, PathBuf)> {
    if let Some(path) = config_flow_arg {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read config flow {}", path.display()))?;
        return Ok((text, path));
    }

    let manifest_path = manifests.first().ok_or_else(|| {
        anyhow::anyhow!(
            "config mode requires --config-flow or a component manifest with dev_flows.{}",
            flow_name
        )
    })?;
    resolve_config_flow_from_manifest(manifest_path, flow_name)
}

fn resolve_config_flow_from_manifest(
    manifest_path: &Path,
    flow_name: &str,
) -> Result<(String, PathBuf)> {
    let manifest_text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let manifest_json: serde_json::Value =
        serde_json::from_str(&manifest_text).context("parse manifest JSON")?;
    let default_graph = manifest_json
        .get("dev_flows")
        .and_then(|v| v.get(flow_name))
        .and_then(|v| v.get("graph"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("manifest missing dev_flows.{}.graph", flow_name))?;
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
    let schema_path =
        ensure_config_schema_path().context("prepare embedded flow schema for config flows")?;
    Ok((yaml, schema_path))
}

fn load_manifest_json(path: &Path) -> Result<serde_json::Value> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read manifest {}", path.display()))?;
    serde_json::from_str(&text).context("parse manifest JSON")
}

fn resolve_manifest_path_for_component(component: &str) -> Result<PathBuf> {
    if component.starts_with("oci://")
        || component.starts_with("repo://")
        || component.starts_with("store://")
    {
        validate_component_ref(component)?;
        let source = classify_remote_source(component, None);
        return resolve_component_manifest_path(&source, Path::new("."));
    }

    let raw = component.strip_prefix("file://").unwrap_or(component);
    let path = PathBuf::from(raw);
    if !path.exists() {
        anyhow::bail!("component path {} not found", path.display());
    }
    if path.is_dir() {
        let manifest_path = path.join("component.manifest.json");
        if !manifest_path.exists() {
            anyhow::bail!(
                "component.manifest.json not found at {}",
                manifest_path.display()
            );
        }
        return Ok(manifest_path);
    }
    if path.is_file() {
        return Ok(path);
    }
    anyhow::bail!(
        "component path {} is not a file or directory",
        path.display()
    )
}

fn questions_for_operation(
    manifest: &serde_json::Value,
    operation: &str,
) -> Result<(Vec<Question>, Option<String>)> {
    if let Some(graph) = dev_flow_graph_from_manifest(manifest, operation)? {
        let questions = extract_questions_from_flow(&graph)?;
        return Ok((questions, Some(operation.to_string())));
    }
    if let Some(graph) = dev_flow_graph_from_manifest(manifest, "default")? {
        let questions = extract_questions_from_flow(&graph)?;
        return Ok((questions, Some("default".to_string())));
    }
    Ok((Vec::new(), None))
}

fn dev_flow_graph_from_manifest(
    manifest: &serde_json::Value,
    flow_name: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(graph) = manifest
        .get("dev_flows")
        .and_then(|v| v.get(flow_name))
        .and_then(|v| v.get("graph"))
        .cloned()
    else {
        return Ok(None);
    };
    Ok(Some(graph))
}

fn questions_from_manifest(manifest_path: &Path, flow_name: &str) -> Result<Vec<Question>> {
    let manifest = load_manifest_json(manifest_path)?;
    let Some(graph) = dev_flow_graph_from_manifest(&manifest, flow_name)? else {
        return Ok(Vec::new());
    };
    extract_questions_from_flow(&graph)
}

fn questions_from_config_flow_text(text: &str) -> Result<Vec<Question>> {
    let flow_value: serde_json::Value =
        serde_yaml_bw::from_str(text).context("parse config flow as YAML")?;
    extract_questions_from_flow(&flow_value)
}

fn validate_example_against_schema(
    schema: &serde_json::Value,
    example: &serde_json::Value,
) -> Result<()> {
    let compiled = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(schema)
        .context("compile answers schema")?;
    if let Err(error) = compiled.validate(example) {
        let messages = error.to_string();
        anyhow::bail!("generated example does not validate against schema: {messages}");
    }
    Ok(())
}

fn write_json_file(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value).context("serialize json")?;
    text.push('\n');
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn print_json_payload(value: &serde_json::Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value).context("write json")?;
    writeln!(stdout).context("write newline")?;
    Ok(())
}

fn answers_to_json_map(answers: QuestionAnswers) -> serde_json::Map<String, serde_json::Value> {
    answers.into_iter().collect()
}

fn answers_to_value(answers: &QuestionAnswers) -> Option<serde_json::Value> {
    if answers.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(
            answers.clone().into_iter().collect(),
        ))
    }
}

fn wizard_header(component: &str, mode: &str) -> String {
    format!("== {component} ({mode}) ==")
}

fn warn_deprecated_wizard_mode(mode: WizardModeArg) -> Option<serde_json::Value> {
    if matches!(mode, WizardModeArg::UpgradeDeprecated) {
        eprintln!(
            "warning: wizard mode 'upgrade' is deprecated; use 'update' (will be removed in a future release)"
        );
        return Some(json!({
            "kind": "deprecation",
            "field": "mode",
            "old": "upgrade",
            "new": "update",
            "message": "wizard mode 'upgrade' is deprecated; use 'update' (will be removed in a future release)"
        }));
    }
    None
}

fn print_json_payload_with_optional_diagnostic(
    mut payload: serde_json::Value,
    diagnostic: Option<&serde_json::Value>,
) -> Result<()> {
    if let Some(diag) = diagnostic
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "diagnostics".to_string(),
            serde_json::Value::Array(vec![diag.clone()]),
        );
    }
    print_json_payload(&payload)
}

fn normalize_capability_group(raw: &str) -> String {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return normalized;
    }
    if let Some(rest) = normalized.strip_prefix("wasi.") {
        let head = rest.split(['.', ':', '/']).next().unwrap_or(rest);
        return format!("wasi:{head}");
    }
    if let Some(rest) = normalized.strip_prefix("host.") {
        let head = rest.split(['.', ':', '/']).next().unwrap_or(rest);
        return format!("host:{head}");
    }
    if normalized.contains(':') {
        return normalized;
    }
    if let Some((left, right)) = normalized.split_once('.') {
        let head = right.split(['.', ':', '/']).next().unwrap_or(right);
        return format!("{left}:{head}");
    }
    normalized
}

fn grouped_capabilities(caps: &[String]) -> Vec<String> {
    let mut groups = BTreeSet::new();
    for cap in caps {
        groups.insert(normalize_capability_group(cap));
    }
    groups.into_iter().collect()
}

fn maybe_print_capability_summary(
    format: OutputFormat,
    describe: &greentic_types::schemas::component::v0_6_0::ComponentDescribe,
) {
    if !matches!(format, OutputFormat::Human) {
        return;
    }
    let requested = grouped_capabilities(&describe.required_capabilities);
    if !requested.is_empty() {
        println!("requested capabilities: {}", requested.join(", "));
    }
    let provided = grouped_capabilities(&describe.provided_capabilities);
    if !provided.is_empty() {
        println!("provided capabilities: {}", provided.join(", "));
    }
    if std::env::var("RUST_LOG")
        .ok()
        .is_some_and(|v| v.contains("debug") || v.contains("trace"))
    {
        if !describe.required_capabilities.is_empty() {
            println!(
                "requested capabilities (raw): {}",
                describe.required_capabilities.join(", ")
            );
        }
        if !describe.provided_capabilities.is_empty() {
            println!(
                "provided capabilities (raw): {}",
                describe.provided_capabilities.join(", ")
            );
        }
    }
}

fn capability_hint_from_error(
    err: &anyhow::Error,
    describe: Option<&greentic_types::schemas::component::v0_6_0::ComponentDescribe>,
) -> Option<String> {
    let lower = err.to_string().to_ascii_lowercase();
    let inferred = if lower.contains("secret") {
        Some("host:secrets")
    } else if lower.contains("state") {
        Some("host:state")
    } else if lower.contains("http") {
        Some("host:http")
    } else if lower.contains("config") {
        Some("host:config")
    } else {
        None
    };
    if let Some(cap) = inferred {
        return Some(cap.to_string());
    }
    describe.and_then(|d| {
        grouped_capabilities(&d.required_capabilities)
            .into_iter()
            .next()
    })
}

fn wizard_op_from_error(err: &anyhow::Error, fallback: &str) -> String {
    let lower = err.to_string().to_ascii_lowercase();
    if lower.contains("call describe") {
        "describe".to_string()
    } else if lower.contains("call qa-spec") {
        "qa-spec".to_string()
    } else if lower.contains("call apply-answers") {
        "apply-answers".to_string()
    } else {
        fallback.to_string()
    }
}

fn wrap_wizard_error(
    err: anyhow::Error,
    component_id: &str,
    op_fallback: &str,
    describe: Option<&greentic_types::schemas::component::v0_6_0::ComponentDescribe>,
) -> anyhow::Error {
    let op = wizard_op_from_error(&err, op_fallback);
    if let Some(cap) = capability_hint_from_error(&err, describe) {
        anyhow!(
            "component '{component_id}' operation '{op}' failed due to denied host ref; requested capability '{cap}'. hint: grant capability {cap} to this component"
        )
        .context(err)
    } else {
        anyhow!("component '{component_id}' operation '{op}' failed").context(err)
    }
}

fn wizard_mode_legacy_label(mode: wizard_ops::WizardMode) -> Option<&'static str> {
    match mode {
        wizard_ops::WizardMode::Update => Some("upgrade"),
        _ => None,
    }
}

fn wizard_answers_json_path(
    base_dir: &Path,
    flow_id: &str,
    node_id: &str,
    mode: wizard_ops::WizardMode,
) -> PathBuf {
    answers::answers_paths(base_dir, flow_id, node_id, mode.as_str()).json
}

fn wizard_answers_json_path_compat(
    base_dir: &Path,
    flow_id: &str,
    node_id: &str,
    mode: wizard_ops::WizardMode,
) -> Option<PathBuf> {
    let primary = wizard_answers_json_path(base_dir, flow_id, node_id, mode);
    if primary.exists() {
        return Some(primary);
    }
    let legacy = wizard_mode_legacy_label(mode).map(|label| {
        answers::answers_paths(base_dir, flow_id, node_id, label)
            .json
            .to_path_buf()
    });
    legacy.filter(|path| path.exists())
}

fn warn_unknown_keys(answers: &QuestionAnswers, questions: &[Question]) {
    if questions.is_empty() || answers.is_empty() {
        return;
    }
    let mut known = std::collections::BTreeSet::new();
    for q in questions {
        known.insert(q.id.as_str());
    }
    let mut unknown = Vec::new();
    for key in answers.keys() {
        if !known.contains(key.as_str()) {
            unknown.push(key.clone());
        }
    }
    if !unknown.is_empty() {
        eprintln!("warning: unknown answer keys: {}", unknown.join(", "));
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum AddStepMode {
    Default,
    Config,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum WizardModeArg {
    Default,
    Setup,
    Update,
    #[value(name = "upgrade", hide = true)]
    UpgradeDeprecated,
    Remove,
}

impl WizardModeArg {
    fn to_mode(self) -> wizard_ops::WizardMode {
        match self {
            WizardModeArg::Default => wizard_ops::WizardMode::Default,
            WizardModeArg::Setup => wizard_ops::WizardMode::Setup,
            WizardModeArg::Update | WizardModeArg::UpgradeDeprecated => {
                wizard_ops::WizardMode::Update
            }
            WizardModeArg::Remove => wizard_ops::WizardMode::Remove,
        }
    }
}

#[derive(Args, Debug)]
struct AddStepArgs {
    /// Component id to resolve via wizard ops (preferred for new flows).
    #[arg(value_name = "component_id")]
    component_id: Option<String>,
    /// Path to the flow file to modify.
    #[arg(long = "flow")]
    flow_path: PathBuf,
    /// Optional anchor node id; defaults to entrypoint or first node.
    #[arg(long = "after")]
    after: Option<String>,
    /// How to source the node to insert.
    #[arg(long = "mode", value_enum, default_value = "default")]
    mode: AddStepMode,
    /// Optional pack alias for the new node.
    #[arg(long = "pack-alias")]
    pack_alias: Option<String>,
    /// Optional wizard mode (default/setup/update/remove).
    #[arg(long = "wizard-mode", value_enum)]
    wizard_mode: Option<WizardModeArg>,
    /// Optional operation for the new node.
    #[arg(long = "operation")]
    operation: Option<String>,
    /// Payload JSON for the new node (default mode).
    #[arg(long = "payload", default_value = "{}")]
    payload: String,
    /// Routing shorthand: make the new node terminal (out).
    #[arg(long = "routing-out", conflicts_with_all = ["routing_reply", "routing_next", "routing_multi_to", "routing_json", "routing_to_anchor"])]
    routing_out: bool,
    /// Routing shorthand: reply to origin.
    #[arg(long = "routing-reply", conflicts_with_all = ["routing_out", "routing_next", "routing_multi_to", "routing_json", "routing_to_anchor"])]
    routing_reply: bool,
    /// Route to a specific node id.
    #[arg(long = "routing-next", conflicts_with_all = ["routing_out", "routing_reply", "routing_multi_to", "routing_json"])]
    routing_next: Option<String>,
    /// Route to multiple node ids (comma-separated).
    #[arg(long = "routing-multi-to", conflicts_with_all = ["routing_out", "routing_reply", "routing_next", "routing_json"])]
    routing_multi_to: Option<String>,
    /// Explicit routing JSON file (escape hatch).
    #[arg(long = "routing-json", conflicts_with_all = ["routing_out", "routing_reply", "routing_next", "routing_multi_to"])]
    routing_json: Option<PathBuf>,
    /// Explicitly thread to the anchor’s existing targets (default if no routing flag is given).
    #[arg(long = "routing-to-anchor", conflicts_with_all = ["routing_out", "routing_reply", "routing_next", "routing_multi_to", "routing_json"])]
    routing_to_anchor: bool,
    /// Config flow file to execute (config mode).
    #[arg(long = "config-flow")]
    config_flow: Option<PathBuf>,
    /// Answers JSON for config mode.
    #[arg(long = "answers")]
    answers: Option<String>,
    /// Answers file (JSON) for config mode.
    #[arg(long = "answers-file")]
    answers_file: Option<PathBuf>,
    /// Directory for wizard answers artifacts.
    #[arg(long = "answers-dir")]
    answers_dir: Option<PathBuf>,
    /// Overwrite existing answers artifacts.
    #[arg(long = "overwrite-answers")]
    overwrite_answers: bool,
    /// Force re-asking wizard questions even if answers exist.
    #[arg(long = "reask")]
    reask: bool,
    /// Locale (BCP47) for wizard prompts.
    #[arg(long = "locale")]
    locale: Option<String>,
    /// Allow interactive QA prompts (wizard mode only).
    #[arg(long = "interactive")]
    interactive: bool,
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
    /// Optional component manifest paths for catalog validation or config flow discovery.
    #[arg(long = "manifest")]
    manifests: Vec<PathBuf>,
    /// Optional node id override.
    #[arg(long = "node-id")]
    node_id: Option<String>,
    /// Remote component reference (oci://, repo://, store://, etc.) for sidecar binding.
    #[arg(long = "component")]
    component_ref: Option<String>,
    /// Local wasm path for sidecar binding (relative to the flow file).
    #[arg(long = "local-wasm")]
    local_wasm: Option<PathBuf>,
    /// Distributor URL for component-id resolution.
    #[arg(long = "distributor-url")]
    distributor_url: Option<String>,
    /// Distributor auth token (optional).
    #[arg(long = "auth-token")]
    auth_token: Option<String>,
    /// Tenant id for component-id resolution.
    #[arg(long = "tenant")]
    tenant: Option<String>,
    /// Environment id for component-id resolution.
    #[arg(long = "env")]
    env: Option<String>,
    /// Pack id for component-id resolution.
    #[arg(long = "pack")]
    pack: Option<String>,
    /// Component version for component-id resolution.
    #[arg(long = "component-version")]
    component_version: Option<String>,
    /// ABI version override for wizard ops.
    #[arg(long = "abi-version")]
    abi_version: Option<String>,
    /// Resolver override (fixture://...) for tests/CI.
    #[arg(long = "resolver")]
    resolver: Option<String>,
    /// Pin the component (resolve tag to digest or hash local wasm).
    #[arg(long = "pin")]
    pin: bool,
    /// Allow contract drift when describe_hash changes.
    #[arg(long = "allow-contract-change")]
    allow_contract_change: bool,
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

fn build_routing_value(args: &AddStepArgs) -> Result<(Option<serde_json::Value>, bool)> {
    if let Some(path) = &args.routing_json {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read routing json {}", path.display()))?;
        let parsed: serde_json::Value =
            serde_json::from_str(&text).context("parse --routing-json as JSON")?;
        return Ok((Some(parsed), false));
    }
    if args.routing_out {
        return Ok((Some(serde_json::Value::String("out".to_string())), false));
    }
    if args.routing_reply {
        return Ok((Some(serde_json::Value::String("reply".to_string())), false));
    }
    if let Some(next) = &args.routing_next {
        return Ok((Some(json!([{ "to": next }])), false));
    }
    if let Some(multi) = &args.routing_multi_to {
        let targets: Vec<_> = multi
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if targets.is_empty() {
            anyhow::bail!("--routing-multi-to requires at least one target");
        }
        let routes: Vec<_> = targets.into_iter().map(|t| json!({ "to": t })).collect();
        return Ok((Some(serde_json::Value::Array(routes)), false));
    }
    // Default: thread to anchor routes (placeholder-based internally).
    let placeholder = json!([{ "to": greentic_flow::splice::NEXT_NODE_PLACEHOLDER }]);
    Ok((Some(placeholder), true))
}

fn build_update_routing(
    args: &UpdateStepArgs,
) -> Result<Option<Vec<greentic_flow::flow_ir::Route>>> {
    if let Some(path) = &args.routing_json {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read routing json {}", path.display()))?;
        let routes = parse_routing_arg(&text)?;
        return Ok(Some(routes));
    }
    if args.routing_out {
        return Ok(Some(vec![greentic_flow::flow_ir::Route {
            out: true,
            ..greentic_flow::flow_ir::Route::default()
        }]));
    }
    if args.routing_reply {
        return Ok(Some(vec![greentic_flow::flow_ir::Route {
            reply: true,
            ..greentic_flow::flow_ir::Route::default()
        }]));
    }
    if let Some(next) = &args.routing_next {
        return Ok(Some(vec![greentic_flow::flow_ir::Route {
            to: Some(next.clone()),
            ..greentic_flow::flow_ir::Route::default()
        }]));
    }
    if let Some(multi) = &args.routing_multi_to {
        let targets: Vec<_> = multi
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if targets.is_empty() {
            anyhow::bail!("--routing-multi-to requires at least one target");
        }
        let routes = targets
            .into_iter()
            .map(|t| greentic_flow::flow_ir::Route {
                to: Some(t.to_string()),
                ..greentic_flow::flow_ir::Route::default()
            })
            .collect();
        return Ok(Some(routes));
    }
    Ok(None)
}

fn infer_node_id_hint(args: &AddStepArgs) -> Option<String> {
    if let Some(explicit) = args.node_id.clone() {
        return Some(explicit);
    }
    if let Some(comp_ref) = &args.component_ref {
        let trimmed = comp_ref
            .trim_start_matches("oci://")
            .trim_start_matches("repo://")
            .trim_start_matches("store://");
        let last = trimmed.rsplit(['/', '\\']).next()?;
        let base = last.split([':', '@']).next().unwrap_or(last);
        if !base.is_empty() {
            return Some(base.replace('_', "-"));
        }
    }
    if let Some(path) = &args.local_wasm
        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
    {
        let normalized = stem.replace('_', "-");
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    None
}

fn resolve_step_id(
    step: Option<String>,
    component_id: Option<&String>,
    meta: &Option<serde_json::Value>,
) -> Result<String> {
    if let Some(step) = step {
        return Ok(step);
    }
    if let Some(component_id) = component_id {
        return flow_meta::find_node_for_component(meta, component_id);
    }
    anyhow::bail!("--step or component_id is required")
}

fn handle_add_step(
    args: AddStepArgs,
    schema_mode: SchemaMode,
    format: OutputFormat,
    backup: bool,
) -> Result<()> {
    let (routing_value, require_placeholder) = build_routing_value(&args)?;
    let component_identity = args
        .component_id
        .clone()
        .or_else(|| args.component_ref.clone())
        .or_else(|| {
            args.local_wasm
                .as_ref()
                .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "component".to_string());

    let wizard_requested = args.component_id.is_some() || args.wizard_mode.is_some();
    if wizard_requested {
        let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
        let doc = load_ygtc_from_path(&args.flow_path)?;
        let flow_ir = FlowIr::from_doc(doc)?;
        let wizard_mode_arg = args.wizard_mode.unwrap_or(WizardModeArg::Default);
        let deprecation_diagnostic = warn_deprecated_wizard_mode(wizard_mode_arg);
        let wizard_mode = wizard_mode_arg.to_mode();
        let resolved = resolve_wizard_component(
            &args.flow_path,
            wizard_mode,
            args.local_wasm.as_ref(),
            args.component_ref.as_ref(),
            args.component_id.as_ref(),
            args.resolver.as_ref(),
            args.distributor_url.as_ref(),
            args.auth_token.as_ref(),
            args.tenant.as_ref(),
            args.env.as_ref(),
            args.pack.as_ref(),
            args.component_version.as_ref(),
        )?;
        let spec = if let Some(fixture) = resolved.fixture.as_ref() {
            wizard_ops::WizardSpecOutput {
                abi: fixture.abi,
                describe_cbor: fixture.describe_cbor.clone(),
                qa_spec_cbor: fixture.qa_spec_cbor.clone(),
            }
        } else {
            wizard_ops::fetch_wizard_spec(&resolved.wasm_bytes, wizard_mode)
                .map_err(|err| wrap_wizard_error(err, &component_identity, "describe", None))?
        };
        let qa_spec = wizard_ops::decode_component_qa_spec(&spec.qa_spec_cbor, wizard_mode)?;
        let describe_for_caps = contracts::decode_component_describe(&spec.describe_cbor).ok();
        let (catalog, locale) = default_i18n_catalog(args.locale.as_deref());

        let mut answers = parse_answers_map(args.answers.as_deref(), args.answers_file.as_deref())?;
        wizard_ops::merge_default_answers(&qa_spec, &mut answers);
        if !qa_spec.questions.is_empty() {
            qa_runner::warn_unknown_keys(&answers, &qa_spec);
            println!(
                "{}",
                wizard_header(&component_identity, wizard_mode.as_str())
            );
            if let Some(describe) = describe_for_caps.as_ref() {
                maybe_print_capability_summary(format, describe);
            }
            if args.interactive {
                answers = qa_runner::run_interactive(&qa_spec, &catalog, &locale, answers)?;
            } else {
                qa_runner::validate_required(&qa_spec, &catalog, &locale, &answers)?;
            }
        }

        let answers_cbor = wizard_ops::answers_to_cbor(&answers)?;
        let current_config = wizard_ops::empty_cbor_map();
        let config_cbor = if let Some(fixture) = resolved.fixture.as_ref() {
            fixture.apply_answers_cbor.clone()
        } else {
            wizard_ops::apply_wizard_answers(
                &resolved.wasm_bytes,
                spec.abi,
                wizard_mode,
                &current_config,
                &answers_cbor,
            )
            .map_err(|err| {
                wrap_wizard_error(
                    err,
                    &component_identity,
                    "apply-answers",
                    describe_for_caps.as_ref(),
                )
            })?
        };
        let operation_id = args.operation.clone().unwrap_or_else(|| "run".to_string());
        let (describe, contract_meta) = derive_contract_meta(&spec.describe_cbor, &operation_id)?;
        validate_config_schema(&describe, &config_cbor)?;
        let config_json = wizard_ops::cbor_to_json(&config_cbor)?;

        let operation = operation_id;
        let routing_json = routing_value
            .clone()
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let component_id_label = component_identity.clone();
        let node_value = json!({
            "component.exec": {
                "component": component_id_label,
                "config": config_json
            },
            "operation": operation,
            "routing": routing_json
        });

        let mut node_id_hint =
            infer_node_id_hint(&args).or_else(|| Some(component_identity.clone()));
        if args.node_id.is_none() {
            node_id_hint = normalize_node_id_hint(node_id_hint, &node_value);
        }

        let spec_plan = AddStepSpec {
            after: args.after.clone(),
            node_id_hint,
            node: node_value,
            allow_cycles: args.allow_cycles,
            require_placeholder,
        };

        let empty_paths: Vec<PathBuf> = Vec::new();
        let empty_catalog = ManifestCatalog::load_from_paths(&empty_paths);
        let plan = plan_add_step(&flow_ir, spec_plan, &empty_catalog)
            .map_err(|diags| anyhow::anyhow!("planning failed: {:?}", diags))?;
        let inserted_id = plan.new_node.id.clone();
        if let Some(stored) = existing_describe_hash(&flow_ir.meta, &inserted_id)
            && stored != contract_meta.describe_hash
            && !args.allow_contract_change
        {
            anyhow::bail!(
                "describe_hash drift for node '{}': stored {} != new {} (use --allow-contract-change to override)",
                inserted_id,
                stored,
                contract_meta.describe_hash
            );
        }
        let mut updated = apply_and_validate(&flow_ir, plan, &empty_catalog, args.allow_cycles)?;

        let abi_version = args
            .abi_version
            .clone()
            .unwrap_or_else(|| wizard_ops::abi_version_from_abi(spec.abi));
        flow_meta::set_component_entry(
            &mut updated.meta,
            &inserted_id,
            &component_identity,
            &abi_version,
            resolved.digest.as_deref(),
            &wizard_ops::describe_exports_for_meta(spec.abi),
            Some(&contract_meta),
        );
        flow_meta::ensure_hints_empty(&mut updated.meta, &inserted_id);

        let updated_doc = updated.to_doc()?;
        let mut output = serde_yaml_bw::to_string(&updated_doc)?;
        if !output.ends_with('\n') {
            output.push('\n');
        }

        if args.validate_only {
            if matches!(format, OutputFormat::Json) {
                let payload = json!({"ok": true, "action": "add-step", "validate_only": true});
                print_json_payload_with_optional_diagnostic(
                    payload,
                    deprecation_diagnostic.as_ref(),
                )?;
            } else {
                println!("add-step validation succeeded");
            }
            return Ok(());
        }

        if !args.dry_run {
            let mut sorted = std::collections::BTreeMap::new();
            for (key, value) in &answers {
                sorted.insert(key.clone(), value.clone());
            }
            let base_dir = answers_base_dir(&args.flow_path, args.answers_dir.as_deref());
            let _paths = answers::write_answers(
                &base_dir,
                &flow_ir.id,
                &inserted_id,
                wizard_mode.as_str(),
                &sorted,
                args.overwrite_answers,
            )?;
            wizard_state::update_wizard_state(
                &args.flow_path,
                &flow_ir.id,
                &inserted_id,
                wizard_mode.as_str(),
                &locale,
            )?;
            write_flow_file(&args.flow_path, &output, true, backup)?;
            sidecar.nodes.insert(
                inserted_id.clone(),
                NodeResolveV1 {
                    source: resolved.source,
                    mode: None,
                },
            );
            write_sidecar(&sidecar_path, &sidecar)?;
            if let Err(err) =
                write_flow_resolve_summary_for_node(&args.flow_path, &inserted_id, &sidecar)
                    .with_context(|| {
                        format!("update resolve summary for {}", args.flow_path.display())
                    })
            {
                eprintln!("warning: {err}");
            }
            if matches!(format, OutputFormat::Json) {
                let payload = json!({
                    "ok": true,
                    "action": "add-step",
                    "node_id": inserted_id,
                    "flow_path": args.flow_path.display().to_string()
                });
                print_json_payload_with_optional_diagnostic(
                    payload,
                    deprecation_diagnostic.as_ref(),
                )?;
            } else {
                println!(
                    "Inserted node after '{}' and wrote {}",
                    args.after.unwrap_or_else(|| "<default anchor>".to_string()),
                    args.flow_path.display()
                );
            }
        } else if matches!(format, OutputFormat::Json) {
            let payload =
                json!({"ok": true, "action": "add-step", "dry_run": true, "flow": output});
            print_json_payload_with_optional_diagnostic(payload, deprecation_diagnostic.as_ref())?;
        } else {
            print!("{output}");
        }

        return Ok(());
    }
    let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
    let (component_source, resolve_mode) = resolve_component_source_inputs(
        args.local_wasm.as_ref(),
        args.component_ref.as_ref(),
        args.pin,
        &args.flow_path,
    )?;
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    let manifest_path_for_schema = args
        .manifests
        .first()
        .cloned()
        .or_else(|| resolve_component_manifest_path(&component_source, &args.flow_path).ok());
    let mut manifest_paths = args.manifests.clone();
    if args.mode == AddStepMode::Config
        && args.config_flow.is_none()
        && manifest_paths.is_empty()
        && let Some(path) = manifest_path_for_schema.clone()
    {
        manifest_paths.push(path);
    }
    if args.mode == AddStepMode::Config && args.config_flow.is_none() && manifest_paths.is_empty() {
        anyhow::bail!(
            "config mode requires --config-flow or a component manifest to provide dev_flows.custom"
        );
    }
    let catalog = ManifestCatalog::load_from_paths(&manifest_paths);

    let mut answers = parse_answers_map(args.answers.as_deref(), args.answers_file.as_deref())?;
    let has_answer_inputs = args.answers.is_some() || args.answers_file.is_some();
    let (mode_input, require_placeholder_flag) = match args.mode {
        AddStepMode::Default => {
            let mut payload_json: serde_json::Value =
                serde_json::from_str(&args.payload).context("parse --payload as JSON")?;
            let mut used_writes = false;
            let mut used_dev_flow = false;
            if let Some(manifest_path) = &manifest_path_for_schema {
                let questions = questions_from_manifest(manifest_path, "default")?;
                if !questions.is_empty() {
                    warn_unknown_keys(&answers, &questions);
                    println!("{}", wizard_header(&component_identity, "default"));
                    if has_answer_inputs {
                        validate_required(&questions, &answers)?;
                    } else {
                        answers = run_interactive_with_seed(&questions, answers)?;
                    }
                    if questions.iter().any(|q| q.writes_to.is_some()) {
                        payload_json = apply_writes_to(payload_json, &questions, &answers)?;
                        used_writes = true;
                    }
                    used_dev_flow = true;
                }
            }
            let operation = args.operation.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "--operation is required in default mode (component id is not stored in flows)"
                )
            })?;
            if !used_writes {
                payload_json = merge_payload(payload_json, answers_to_value(&answers));
            }
            if !used_dev_flow && let Some(manifest_path) = &manifest_path_for_schema {
                let schema_resolution = resolve_input_schema(manifest_path, &operation)?;
                let schema_present = require_schema(
                    schema_mode,
                    &schema_resolution.component_id,
                    &schema_resolution.operation,
                    &schema_resolution.manifest_path,
                    "operations[].input_schema",
                    schema_resolution.schema.as_ref(),
                )?;
                if schema_present.is_some() {
                    validate_payload_against_schema(&schema_resolution, &payload_json)?;
                }
            }
            let routing_json = routing_value.clone();
            (
                AddStepModeInput::Default {
                    operation,
                    payload: payload_json,
                    routing: routing_json,
                },
                require_placeholder,
            )
        }
        AddStepMode::Config => {
            let (config_flow, schema_path) =
                resolve_config_flow(args.config_flow.clone(), &manifest_paths, "custom")?;
            let questions = questions_from_config_flow_text(&config_flow)?;
            if !questions.is_empty() {
                warn_unknown_keys(&answers, &questions);
                println!("{}", wizard_header(&component_identity, "config"));
                if has_answer_inputs {
                    validate_required(&questions, &answers)?;
                } else {
                    answers = run_interactive_with_seed(&questions, answers)?;
                }
            }
            let manifest_path_for_validation = manifest_paths.first().cloned().or_else(|| {
                resolve_component_manifest_path(&component_source, &args.flow_path).ok()
            });
            (
                AddStepModeInput::Config {
                    config_flow,
                    schema_path: schema_path.into_boxed_path(),
                    answers: answers_to_json_map(answers),
                    manifest_id: Some(component_identity.clone()),
                    manifest_path: manifest_path_for_validation,
                },
                true,
            )
        }
    };

    let (hint, node_value) = materialize_node(mode_input, &catalog)?;
    let mut node_id_hint = infer_node_id_hint(&args);
    if node_id_hint.is_none() {
        node_id_hint = hint;
    }
    if args.node_id.is_none() {
        node_id_hint = normalize_node_id_hint(node_id_hint, &node_value);
    }

    let spec = AddStepSpec {
        after: args.after.clone(),
        node_id_hint,
        node: node_value,
        allow_cycles: args.allow_cycles,
        require_placeholder: require_placeholder_flag,
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
        if matches!(format, OutputFormat::Json) {
            let payload = json!({"ok": true, "action": "add-step", "validate_only": true});
            print_json_payload(&payload)?;
        } else {
            println!("add-step validation succeeded");
        }
        return Ok(());
    }

    if !args.dry_run {
        write_flow_file(&args.flow_path, &output, true, backup)?;
        sidecar.nodes.insert(
            inserted_id.clone(),
            NodeResolveV1 {
                source: component_source,
                mode: resolve_mode,
            },
        );
        write_sidecar(&sidecar_path, &sidecar)?;
        if let Err(err) =
            write_flow_resolve_summary_for_node(&args.flow_path, &inserted_id, &sidecar)
                .with_context(|| format!("update resolve summary for {}", args.flow_path.display()))
        {
            eprintln!("warning: {err}");
        }
        if matches!(format, OutputFormat::Json) {
            let payload = json!({
                "ok": true,
                "action": "add-step",
                "node_id": inserted_id,
                "flow_path": args.flow_path.display().to_string()
            });
            print_json_payload(&payload)?;
        } else {
            println!(
                "Inserted node after '{}' and wrote {}",
                args.after.unwrap_or_else(|| "<default anchor>".to_string()),
                args.flow_path.display()
            );
        }
    } else if matches!(format, OutputFormat::Json) {
        let payload = json!({"ok": true, "action": "add-step", "dry_run": true, "flow": output});
        print_json_payload(&payload)?;
    } else {
        print!("{output}");
    }

    Ok(())
}

fn handle_update_step(
    args: UpdateStepArgs,
    schema_mode: SchemaMode,
    format: OutputFormat,
    backup: bool,
) -> Result<()> {
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let mut flow_ir = FlowIr::from_doc(doc)?;
    let component_identity = args
        .component_id
        .clone()
        .or_else(|| args.component.clone())
        .or_else(|| {
            args.local_wasm
                .as_ref()
                .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "component".to_string());
    let step_id = resolve_step_id(args.step.clone(), args.component_id.as_ref(), &flow_ir.meta)?;
    let wizard_requested = args.component_id.is_some() || args.wizard_mode.is_some();
    if wizard_requested {
        let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
        let wizard_mode_arg = args.wizard_mode.unwrap_or(WizardModeArg::Update);
        let deprecation_diagnostic = warn_deprecated_wizard_mode(wizard_mode_arg);
        let wizard_mode = wizard_mode_arg.to_mode();
        let resolved = resolve_wizard_component(
            &args.flow_path,
            wizard_mode,
            args.local_wasm.as_ref(),
            args.component.as_ref(),
            args.component_id.as_ref(),
            args.resolver.as_ref(),
            args.distributor_url.as_ref(),
            args.auth_token.as_ref(),
            args.tenant.as_ref(),
            args.env.as_ref(),
            args.pack.as_ref(),
            args.component_version.as_ref(),
        )?;
        let spec = if let Some(fixture) = resolved.fixture.as_ref() {
            wizard_ops::WizardSpecOutput {
                abi: fixture.abi,
                describe_cbor: fixture.describe_cbor.clone(),
                qa_spec_cbor: fixture.qa_spec_cbor.clone(),
            }
        } else {
            wizard_ops::fetch_wizard_spec(&resolved.wasm_bytes, wizard_mode)
                .map_err(|err| wrap_wizard_error(err, &component_identity, "describe", None))?
        };
        let qa_spec = wizard_ops::decode_component_qa_spec(&spec.qa_spec_cbor, wizard_mode)?;
        let describe_for_caps = contracts::decode_component_describe(&spec.describe_cbor).ok();
        let (catalog, locale) = default_i18n_catalog(args.locale.as_deref());

        let base_dir = answers_base_dir(&args.flow_path, args.answers_dir.as_deref());
        let fallback_path = if !args.reask && args.answers.is_none() && args.answers_file.is_none()
        {
            wizard_answers_json_path_compat(&base_dir, &flow_ir.id, &step_id, wizard_mode)
        } else {
            None
        };
        let answers_file = args.answers_file.as_deref().or(fallback_path.as_deref());
        let mut answers = parse_answers_map(args.answers.as_deref(), answers_file)?;
        wizard_ops::merge_default_answers(&qa_spec, &mut answers);
        if !qa_spec.questions.is_empty() {
            qa_runner::warn_unknown_keys(&answers, &qa_spec);
            println!(
                "{}",
                wizard_header(&component_identity, wizard_mode.as_str())
            );
            if let Some(describe) = describe_for_caps.as_ref() {
                maybe_print_capability_summary(format, describe);
            }
            if args.interactive {
                answers = qa_runner::run_interactive(&qa_spec, &catalog, &locale, answers)?;
            } else {
                qa_runner::validate_required(&qa_spec, &catalog, &locale, &answers)?;
            }
        }

        let answers_cbor = wizard_ops::answers_to_cbor(&answers)?;
        let mut node = flow_ir
            .nodes
            .get(&step_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("step '{}' not found", step_id))?;
        let current_config = wizard_ops::json_to_cbor(&node.payload)?;
        let config_cbor = if let Some(fixture) = resolved.fixture.as_ref() {
            fixture.apply_answers_cbor.clone()
        } else {
            wizard_ops::apply_wizard_answers(
                &resolved.wasm_bytes,
                spec.abi,
                wizard_mode,
                &current_config,
                &answers_cbor,
            )
            .map_err(|err| {
                wrap_wizard_error(
                    err,
                    &component_identity,
                    "apply-answers",
                    describe_for_caps.as_ref(),
                )
            })?
        };
        let mut new_operation = node.operation.clone();
        if let Some(op) = args.operation.clone() {
            new_operation = op;
        }
        let (describe, contract_meta) = derive_contract_meta(&spec.describe_cbor, &new_operation)?;
        if let Some(stored) = existing_describe_hash(&flow_ir.meta, &step_id)
            && stored != contract_meta.describe_hash
            && !args.allow_contract_change
        {
            anyhow::bail!(
                "describe_hash drift for node '{}': stored {} != new {} (use --allow-contract-change to override)",
                step_id,
                stored,
                contract_meta.describe_hash
            );
        }
        validate_config_schema(&describe, &config_cbor)?;
        let config_json = wizard_ops::cbor_to_json(&config_cbor)?;
        node.payload = config_json;
        node.operation = new_operation.clone();
        if let Some(routing) = build_update_routing(&args)? {
            node.routing = routing;
        }
        flow_ir.nodes.insert(step_id.clone(), node);

        let abi_version = args
            .abi_version
            .clone()
            .unwrap_or_else(|| wizard_ops::abi_version_from_abi(spec.abi));
        flow_meta::set_component_entry(
            &mut flow_ir.meta,
            &step_id,
            &component_identity,
            &abi_version,
            resolved.digest.as_deref(),
            &wizard_ops::describe_exports_for_meta(spec.abi),
            Some(&contract_meta),
        );
        flow_meta::ensure_hints_empty(&mut flow_ir.meta, &step_id);

        let doc_out = flow_ir.to_doc()?;
        let yaml = serialize_doc(&doc_out)?;
        load_ygtc_from_str(&yaml)?;
        if !args.dry_run {
            let mut sorted = std::collections::BTreeMap::new();
            for (key, value) in &answers {
                sorted.insert(key.clone(), value.clone());
            }
            let _paths = answers::write_answers(
                &base_dir,
                &flow_ir.id,
                &step_id,
                wizard_mode.as_str(),
                &sorted,
                args.overwrite_answers,
            )?;
            wizard_state::update_wizard_state(
                &args.flow_path,
                &flow_ir.id,
                &step_id,
                wizard_mode.as_str(),
                &locale,
            )?;
            write_flow_file(&args.flow_path, &yaml, true, backup)?;
            sidecar.nodes.insert(
                step_id.clone(),
                NodeResolveV1 {
                    source: resolved.source,
                    mode: None,
                },
            );
            write_sidecar(&sidecar_path, &sidecar)?;
            if let Err(err) =
                write_flow_resolve_summary_for_node(&args.flow_path, &step_id, &sidecar)
                    .with_context(|| {
                        format!("update resolve summary for {}", args.flow_path.display())
                    })
            {
                eprintln!("warning: {err}");
            }
            if matches!(format, OutputFormat::Json) {
                let payload = json!({
                    "ok": true,
                    "action": "update-step",
                    "node_id": step_id,
                    "flow_path": args.flow_path.display().to_string()
                });
                print_json_payload_with_optional_diagnostic(
                    payload,
                    deprecation_diagnostic.as_ref(),
                )?;
            } else {
                println!("Updated step '{}' in {}", step_id, args.flow_path.display());
            }
        } else if matches!(format, OutputFormat::Json) {
            let payload =
                json!({"ok": true, "action": "update-step", "dry_run": true, "flow": yaml});
            print_json_payload_with_optional_diagnostic(payload, deprecation_diagnostic.as_ref())?;
        } else {
            print!("{yaml}");
        }
        return Ok(());
    }
    let (_sidecar_path, sidecar) = ensure_sidecar(&args.flow_path)?;
    if let Some(component) = args.component.as_deref() {
        validate_component_ref(component)?;
    }
    let sidecar_entry = sidecar.nodes.get(&step_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no sidecar mapping for node '{}'; run greentic-flow bind-component or re-add the step with --component/--local-wasm",
            step_id
        )
    })?;
    let component_payload = load_component_payload(&sidecar_entry.source, &args.flow_path)?;
    let mut node = flow_ir
        .nodes
        .get(&step_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("step '{}' not found", step_id))?;
    let mut merged_payload = node.payload.clone();
    if let Some(component_defaults) = component_payload {
        merged_payload = merge_payload(merged_payload, Some(component_defaults));
    }
    let mut answers = parse_answers_map(args.answers.as_deref(), args.answers_file.as_deref())?;
    let mut new_operation = args
        .operation
        .clone()
        .unwrap_or_else(|| node.operation.clone());
    let new_payload = if args.mode == "config" {
        let manifest_path =
            resolve_component_manifest_path(&sidecar_entry.source, &args.flow_path)?;
        let (config_flow, schema_path) =
            resolve_config_flow(None, std::slice::from_ref(&manifest_path), "custom")?;
        let mut base_answers = QuestionAnswers::new();
        if let Some(obj) = merged_payload.as_object() {
            base_answers.extend(obj.clone());
        }
        base_answers.extend(answers.clone());
        let questions = questions_from_config_flow_text(&config_flow)?;
        if !questions.is_empty() {
            warn_unknown_keys(&answers, &questions);
            println!("{}", wizard_header(&component_identity, "config"));
            if args.non_interactive {
                validate_required(&questions, &base_answers)?;
            } else {
                base_answers = run_interactive_with_seed(&questions, base_answers)?;
            }
        }
        let flow_name = "custom";
        let source_desc = format!("dev_flows.{flow_name}");
        if questions.is_empty() {
            require_schema(
                schema_mode,
                &component_identity,
                flow_name,
                &manifest_path,
                &source_desc,
                None,
            )?;
        } else {
            let dev_schema = schema_for_questions(&questions);
            require_schema(
                schema_mode,
                &component_identity,
                flow_name,
                &manifest_path,
                &source_desc,
                Some(&dev_schema),
            )?;
        }
        let answers_map = answers_to_json_map(base_answers);
        let output = run_config_flow(
            &config_flow,
            &schema_path,
            &answers_map,
            Some(component_identity.clone()),
        )?;
        let normalized = normalize_node_map(output.node)?;
        if args.operation.is_none() {
            new_operation = normalized.operation.clone();
        }
        normalized.payload
    } else if args.mode == "default" {
        let mut payload = merged_payload;
        let mut used_writes = false;
        let mut manifest_path_for_validation: Option<PathBuf> = None;
        if let Ok(manifest_path) =
            resolve_component_manifest_path(&sidecar_entry.source, &args.flow_path)
        {
            manifest_path_for_validation = Some(manifest_path.clone());
            let questions = questions_from_manifest(&manifest_path, "default")?;
            if !questions.is_empty() {
                let mut base_answers = extract_answers_from_payload(&questions, &payload);
                warn_unknown_keys(&answers, &questions);
                base_answers.extend(answers.clone());
                println!("{}", wizard_header(&component_identity, "default"));
                if args.non_interactive {
                    validate_required(&questions, &base_answers)?;
                } else {
                    base_answers = run_interactive_with_seed(&questions, base_answers)?;
                }
                answers = base_answers;
                if questions.iter().any(|q| q.writes_to.is_some()) {
                    payload = apply_writes_to(payload, &questions, &answers)?;
                    used_writes = true;
                }
            }
        }
        let final_payload = if used_writes {
            payload.clone()
        } else {
            merge_payload(payload, answers_to_value(&answers))
        };
        if let Some(manifest_path) = manifest_path_for_validation.as_ref() {
            let schema_resolution = resolve_input_schema(manifest_path, &new_operation)?;
            let schema_present = require_schema(
                schema_mode,
                &schema_resolution.component_id,
                &schema_resolution.operation,
                &schema_resolution.manifest_path,
                "operations[].input_schema",
                schema_resolution.schema.as_ref(),
            )?;
            if schema_present.is_some() {
                validate_payload_against_schema(&schema_resolution, &final_payload)?;
            }
        }
        final_payload
    } else {
        merged_payload
    };
    let new_routing = if let Some(routing) = build_update_routing(&args)? {
        routing
    } else {
        node.routing.clone()
    };

    node.operation = new_operation;
    node.payload = new_payload;
    node.routing = new_routing;
    flow_ir.nodes.insert(step_id.clone(), node);

    let doc_out = flow_ir.to_doc()?;
    // Adjust entrypoint if it targeted the removed node in other ops; here node stays, so no-op.
    let yaml = serialize_doc(&doc_out)?;
    load_ygtc_from_str(&yaml)?; // schema validation
    if !args.dry_run {
        write_flow_file(&args.flow_path, &yaml, true, backup)?;
        if let Err(err) = write_flow_resolve_summary_for_node(&args.flow_path, &step_id, &sidecar)
            .with_context(|| format!("update resolve summary for {}", args.flow_path.display()))
        {
            eprintln!("warning: {err}");
        }
        if matches!(format, OutputFormat::Json) {
            let payload = json!({
                "ok": true,
                "action": "update-step",
                "node_id": step_id,
                "flow_path": args.flow_path.display().to_string()
            });
            print_json_payload(&payload)?;
        } else {
            println!("Updated step '{}' in {}", step_id, args.flow_path.display());
        }
    } else if matches!(format, OutputFormat::Json) {
        let payload = json!({"ok": true, "action": "update-step", "dry_run": true, "flow": yaml});
        print_json_payload(&payload)?;
    } else {
        print!("{yaml}");
    }
    Ok(())
}

fn handle_delete_step(args: DeleteStepArgs, format: OutputFormat, backup: bool) -> Result<()> {
    let (sidecar_path, mut sidecar) = ensure_sidecar(&args.flow_path)?;
    let doc = load_ygtc_from_path(&args.flow_path)?;
    let mut flow_ir = FlowIr::from_doc(doc)?;
    let component_identity = args
        .component_id
        .clone()
        .or_else(|| args.component.clone())
        .or_else(|| {
            args.local_wasm
                .as_ref()
                .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "component".to_string());
    let target = resolve_step_id(args.step.clone(), args.component_id.as_ref(), &flow_ir.meta)?;
    let wizard_requested = args.component_id.is_some() || args.wizard_mode.is_some();
    let mut deprecation_diagnostic: Option<serde_json::Value> = None;
    if wizard_requested {
        let wizard_mode_arg = args.wizard_mode.unwrap_or(WizardModeArg::Remove);
        deprecation_diagnostic = warn_deprecated_wizard_mode(wizard_mode_arg);
        let wizard_mode = wizard_mode_arg.to_mode();
        let resolved = resolve_wizard_component(
            &args.flow_path,
            wizard_mode,
            args.local_wasm.as_ref(),
            args.component.as_ref(),
            args.component_id.as_ref(),
            args.resolver.as_ref(),
            args.distributor_url.as_ref(),
            args.auth_token.as_ref(),
            args.tenant.as_ref(),
            args.env.as_ref(),
            args.pack.as_ref(),
            args.component_version.as_ref(),
        )?;
        let spec = if let Some(fixture) = resolved.fixture.as_ref() {
            wizard_ops::WizardSpecOutput {
                abi: fixture.abi,
                describe_cbor: fixture.describe_cbor.clone(),
                qa_spec_cbor: fixture.qa_spec_cbor.clone(),
            }
        } else {
            wizard_ops::fetch_wizard_spec(&resolved.wasm_bytes, wizard_mode)
                .map_err(|err| wrap_wizard_error(err, &component_identity, "describe", None))?
        };
        let qa_spec = wizard_ops::decode_component_qa_spec(&spec.qa_spec_cbor, wizard_mode)?;
        let describe_for_caps = contracts::decode_component_describe(&spec.describe_cbor).ok();
        let (catalog, locale) = default_i18n_catalog(args.locale.as_deref());

        let base_dir = answers_base_dir(&args.flow_path, args.answers_dir.as_deref());
        let fallback_path = if !args.reask && args.answers.is_none() && args.answers_file.is_none()
        {
            wizard_answers_json_path_compat(&base_dir, &flow_ir.id, &target, wizard_mode)
        } else {
            None
        };
        let answers_file = args.answers_file.as_deref().or(fallback_path.as_deref());
        let mut answers = parse_answers_map(args.answers.as_deref(), answers_file)?;
        wizard_ops::merge_default_answers(&qa_spec, &mut answers);
        if !qa_spec.questions.is_empty() {
            qa_runner::warn_unknown_keys(&answers, &qa_spec);
            println!(
                "{}",
                wizard_header(&component_identity, wizard_mode.as_str())
            );
            if let Some(describe) = describe_for_caps.as_ref() {
                maybe_print_capability_summary(format, describe);
            }
            if args.interactive {
                answers = qa_runner::run_interactive(&qa_spec, &catalog, &locale, answers)?;
            } else {
                qa_runner::validate_required(&qa_spec, &catalog, &locale, &answers)?;
            }
        }

        let answers_cbor = wizard_ops::answers_to_cbor(&answers)?;
        let target_node = flow_ir
            .nodes
            .get(&target)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("step '{}' not found", target))?;
        let current_config = wizard_ops::json_to_cbor(&target_node.payload)?;
        if let Some(fixture) = resolved.fixture.as_ref() {
            let _ = fixture.apply_answers_cbor.clone();
        } else {
            let _ = wizard_ops::apply_wizard_answers(
                &resolved.wasm_bytes,
                spec.abi,
                wizard_mode,
                &current_config,
                &answers_cbor,
            )
            .map_err(|err| {
                wrap_wizard_error(
                    err,
                    &component_identity,
                    "apply-answers",
                    describe_for_caps.as_ref(),
                )
            })?;
        }
        flow_meta::clear_component_entry(&mut flow_ir.meta, &target);
        if args.write {
            let mut sorted = std::collections::BTreeMap::new();
            for (key, value) in &answers {
                sorted.insert(key.clone(), value.clone());
            }
            let _paths = answers::write_answers(
                &base_dir,
                &flow_ir.id,
                &target,
                wizard_mode.as_str(),
                &sorted,
                args.overwrite_answers,
            )?;
            wizard_state::update_wizard_state(
                &args.flow_path,
                &flow_ir.id,
                &target,
                wizard_mode.as_str(),
                &locale,
            )?;
        }
    }

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
    flow_meta::clear_component_entry(&mut flow_ir.meta, &target);
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
        write_flow_file(&args.flow_path, &yaml, true, backup)?;
        sidecar.nodes.remove(&target);
        write_sidecar(&sidecar_path, &sidecar)?;
        let _ = wizard_state::remove_wizard_step(&args.flow_path, &flow_ir.id, &target);
        if let Err(err) = remove_flow_resolve_summary_node(&args.flow_path, &target)
            .with_context(|| format!("update resolve summary for {}", args.flow_path.display()))
        {
            eprintln!("warning: {err}");
        }
        if matches!(format, OutputFormat::Json) {
            let payload = json!({
                "ok": true,
                "action": "delete-step",
                "node_id": target,
                "flow_path": args.flow_path.display().to_string()
            });
            print_json_payload_with_optional_diagnostic(payload, deprecation_diagnostic.as_ref())?;
        } else {
            println!(
                "Deleted step '{}' from {}",
                target,
                args.flow_path.display()
            );
        }
    } else if matches!(format, OutputFormat::Json) {
        let payload = json!({"ok": true, "action": "delete-step", "dry_run": true, "flow": yaml});
        print_json_payload_with_optional_diagnostic(payload, deprecation_diagnostic.as_ref())?;
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
        if let Err(err) = write_flow_resolve_summary_for_node(&args.flow_path, &args.step, &sidecar)
            .with_context(|| format!("update resolve summary for {}", args.flow_path.display()))
        {
            eprintln!("warning: {err}");
        }
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

fn require_schema<'a>(
    mode: SchemaMode,
    component_id: &str,
    operation: &str,
    manifest_path: &Path,
    source_desc: &str,
    schema: Option<&'a serde_json::Value>,
) -> Result<Option<&'a serde_json::Value>> {
    if let Some(schema) = schema {
        if is_effectively_empty_schema(schema) {
            report_empty_schema(mode, component_id, operation, manifest_path, source_desc)?;
            return Ok(None);
        }
        Ok(Some(schema))
    } else {
        report_empty_schema(mode, component_id, operation, manifest_path, source_desc)?;
        Ok(None)
    }
}

fn report_empty_schema(
    mode: SchemaMode,
    component_id: &str,
    operation: &str,
    manifest_path: &Path,
    source_desc: &str,
) -> Result<()> {
    let base = format!(
        "component '{}', operation '{}', schema missing or empty at {} (source: {})",
        component_id,
        operation,
        manifest_path.display(),
        source_desc
    );
    let guidance = schema_guidance();
    match mode {
        SchemaMode::Strict => Err(anyhow!("E_SCHEMA_EMPTY: {base}. {guidance}")),
        SchemaMode::Permissive => {
            eprintln!("W_SCHEMA_EMPTY: {base}. {guidance} Validation disabled (permissive).");
            Ok(())
        }
    }
}

fn parse_answers_map(
    answers: Option<&str>,
    answers_file: Option<&Path>,
) -> Result<QuestionAnswers> {
    let mut merged = QuestionAnswers::new();
    if let Some(path) = answers_file {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read answers file {}", path.display()))?;
        let parsed: serde_json::Value = serde_yaml_bw::from_str(&text)
            .or_else(|_| serde_json::from_str(&text))
            .context("parse answers file as JSON/YAML")?;
        let obj = parsed
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("answers file must contain a JSON/YAML object"))?;
        merged.extend(obj.clone());
    }
    if let Some(text) = answers {
        let parsed: serde_json::Value = serde_yaml_bw::from_str(text)
            .or_else(|_| serde_json::from_str(text))
            .context("parse --answers as JSON/YAML")?;
        let obj = parsed
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("--answers must be a JSON/YAML object"))?;
        merged.extend(obj.clone());
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
        serde_json::from_str(raw).context("parse routing as JSON array or shorthand string")?;
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

struct SidecarValidation {
    path: PathBuf,
    updated: bool,
    missing: Vec<String>,
    extra: Vec<String>,
    invalid: Vec<String>,
}

fn validate_sidecar_for_flow(
    flow_path: &Path,
    flow: &greentic_types::Flow,
    prompt_unused: bool,
    apply_updates: bool,
) -> Result<SidecarValidation> {
    let sidecar_path = sidecar_path_for_flow(flow_path);
    let flow_name = flow_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "flow.ygtc".to_string());
    let node_ids: BTreeSet<String> = flow.nodes.keys().map(|id| id.to_string()).collect();

    if !sidecar_path.exists() {
        if node_ids.is_empty() {
            return Ok(SidecarValidation {
                path: sidecar_path,
                updated: false,
                missing: Vec::new(),
                extra: Vec::new(),
                invalid: Vec::new(),
            });
        }
        return Ok(SidecarValidation {
            path: sidecar_path,
            updated: false,
            missing: node_ids.into_iter().collect(),
            extra: Vec::new(),
            invalid: Vec::new(),
        });
    }

    let mut doc = read_flow_resolve(&sidecar_path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let mut updated = false;
    if apply_updates && doc.flow != flow_name {
        doc.flow = flow_name;
        updated = true;
    }

    let mut missing = Vec::new();
    for id in &node_ids {
        if !doc.nodes.contains_key(id) {
            missing.push(id.clone());
        }
    }

    let mut extra = Vec::new();
    for id in doc.nodes.keys() {
        if !node_ids.contains(id) {
            extra.push(id.clone());
        }
    }

    if prompt_unused && !extra.is_empty() && confirm_delete_unused(&sidecar_path, &extra)? {
        for id in &extra {
            doc.nodes.remove(id);
        }
        updated = true;
        extra.clear();
    }

    let mut invalid = Vec::new();
    for (id, entry) in &doc.nodes {
        if let Err(err) = validate_sidecar_source(&entry.source, flow_path) {
            invalid.push(format!("{id}: {err}"));
        }
    }

    if apply_updates && updated {
        write_sidecar(&sidecar_path, &doc)?;
    }

    Ok(SidecarValidation {
        path: sidecar_path,
        updated,
        missing,
        extra,
        invalid,
    })
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

fn validate_component_ref(reference: &str) -> Result<()> {
    if reference.starts_with("oci://") {
        return validate_oci_reference(reference);
    }
    if reference.starts_with("repo://") || reference.starts_with("store://") {
        let rest = reference
            .split_once("://")
            .map(|(_, tail)| tail)
            .unwrap_or("")
            .trim();
        if rest.is_empty() {
            anyhow::bail!("--component must include a reference after the scheme");
        }
        return Ok(());
    }
    anyhow::bail!("--component must start with oci://, repo://, or store://");
}

fn validate_oci_reference(reference: &str) -> Result<()> {
    let rest = reference.strip_prefix("oci://").unwrap_or("").trim();
    if rest.is_empty() {
        anyhow::bail!("oci:// references must include a registry host and repository");
    }
    let mut parts = rest.splitn(2, '/');
    let host = parts.next().unwrap_or("").trim();
    let repo = parts.next().unwrap_or("").trim();
    if host.is_empty() || repo.is_empty() {
        anyhow::bail!("oci:// references must be in the form oci://<host>/<repo>");
    }
    if host == "localhost"
        || host.starts_with("localhost:")
        || host.starts_with("127.")
        || host.starts_with("0.")
    {
        anyhow::bail!("oci:// references must use a public registry host");
    }
    if !host.contains('.') {
        anyhow::bail!("oci:// references must include a public registry host");
    }
    Ok(())
}

fn validate_sidecar_source(source: &ComponentSourceRefV1, flow_path: &Path) -> Result<()> {
    match source {
        ComponentSourceRefV1::Local { path, .. } => {
            if path.trim().is_empty() {
                anyhow::bail!("local wasm path is empty");
            }
            let abs = local_path_from_sidecar(path, flow_path);
            if !abs.exists() {
                anyhow::bail!("local wasm missing at {}", abs.display());
            }
        }
        ComponentSourceRefV1::Oci { r#ref, .. } => {
            if r#ref.trim().is_empty() {
                anyhow::bail!("oci reference is empty");
            }
            if !r#ref.starts_with("oci://") {
                anyhow::bail!("oci reference must start with oci://");
            }
            validate_oci_reference(r#ref)?;
        }
        ComponentSourceRefV1::Repo { r#ref, .. } => {
            if r#ref.trim().is_empty() {
                anyhow::bail!("repo reference is empty");
            }
            if !r#ref.starts_with("repo://") {
                anyhow::bail!("repo reference must start with repo://");
            }
        }
        ComponentSourceRefV1::Store { r#ref, .. } => {
            if r#ref.trim().is_empty() {
                anyhow::bail!("store reference is empty");
            }
            if !r#ref.starts_with("store://") {
                anyhow::bail!("store reference must start with store://");
            }
        }
    }
    Ok(())
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

fn normalize_local_wasm_path(local: &Path, flow_path: &Path) -> Result<(PathBuf, String)> {
    let raw = local.to_string_lossy();
    let trimmed = raw.strip_prefix("file://").unwrap_or(&raw);
    let raw_path = PathBuf::from(trimmed);
    let flow_dir = flow_path.parent().unwrap_or_else(|| Path::new("."));
    let abs_path = if raw_path.is_absolute() {
        raw_path
    } else {
        let cwd = std::env::current_dir().context("resolve current directory")?;
        cwd.join(raw_path)
    };
    let abs_path = fs::canonicalize(&abs_path)
        .with_context(|| format!("resolve local wasm path {}", abs_path.display()))?;
    let flow_dir = fs::canonicalize(flow_dir)
        .with_context(|| format!("resolve flow directory {}", flow_dir.display()))?;
    let rel_path = diff_paths(&abs_path, &flow_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "failed to compute a relative path from {} to {}",
            flow_dir.display(),
            abs_path.display()
        )
    })?;
    let rel_str = rel_path.to_string_lossy().to_string();
    if rel_str.trim().is_empty() {
        anyhow::bail!("local wasm path resolves to an empty relative path");
    }
    Ok((abs_path, format!("file://{rel_str}")))
}

fn local_path_from_sidecar(path: &str, flow_path: &Path) -> PathBuf {
    let trimmed = path.strip_prefix("file://").unwrap_or(path);
    let raw = PathBuf::from(trimmed);
    if raw.is_absolute() {
        raw
    } else {
        flow_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw)
    }
}

fn resolve_component_source_inputs(
    local_wasm: Option<&PathBuf>,
    component_ref: Option<&String>,
    pin: bool,
    flow_path: &Path,
) -> Result<(ComponentSourceRefV1, Option<ResolveModeV1>)> {
    if let Some(local) = local_wasm {
        let (abs_path, uri_path) = normalize_local_wasm_path(local, flow_path)?;
        let digest = if pin {
            Some(compute_local_digest(&abs_path)?)
        } else {
            None
        };
        let source = ComponentSourceRefV1::Local {
            path: uri_path,
            digest: digest.clone(),
        };
        let mode = digest.as_ref().map(|_| ResolveModeV1::Pinned);
        return Ok((source, mode));
    }

    if let Some(reference) = component_ref {
        validate_component_ref(reference)?;
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

struct WizardComponentResolution {
    wasm_bytes: Vec<u8>,
    digest: Option<String>,
    source: ComponentSourceRefV1,
    fixture: Option<WizardFixture>,
}

struct WizardFixture {
    abi: wizard_ops::WizardAbi,
    describe_cbor: Vec<u8>,
    qa_spec_cbor: Vec<u8>,
    apply_answers_cbor: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
fn resolve_wizard_component(
    flow_path: &Path,
    wizard_mode: wizard_ops::WizardMode,
    local_wasm: Option<&PathBuf>,
    component_ref: Option<&String>,
    component_id: Option<&String>,
    resolver: Option<&String>,
    distributor_url: Option<&String>,
    auth_token: Option<&String>,
    tenant: Option<&String>,
    env: Option<&String>,
    pack: Option<&String>,
    component_version: Option<&String>,
) -> Result<WizardComponentResolution> {
    if let Some(local) = local_wasm {
        let (abs_path, uri_path) = normalize_local_wasm_path(local, flow_path)?;
        let bytes =
            fs::read(&abs_path).with_context(|| format!("read wasm at {}", abs_path.display()))?;
        let digest = Some(compute_local_digest(&abs_path)?);
        let source = ComponentSourceRefV1::Local {
            path: uri_path,
            digest: digest.clone(),
        };
        return Ok(WizardComponentResolution {
            wasm_bytes: bytes,
            digest,
            source,
            fixture: None,
        });
    }

    if let Some(reference) = component_ref {
        if let Some(fixture) = resolve_fixture_wizard(reference, resolver, wizard_mode)? {
            let source = classify_remote_source(reference, None);
            return Ok(WizardComponentResolution {
                wasm_bytes: Vec::new(),
                digest: None,
                source,
                fixture: Some(fixture),
            });
        }
        let resolved = resolve_ref_to_bytes(reference, resolver)?;
        let source = classify_remote_source(reference, resolved.digest.clone());
        return Ok(WizardComponentResolution {
            wasm_bytes: resolved.bytes,
            digest: resolved.digest,
            source,
            fixture: None,
        });
    }

    if let Some(component_id) = component_id {
        let reference = resolve_component_id_reference(
            component_id,
            distributor_url,
            auth_token,
            tenant,
            env,
            pack,
            component_version,
        )?;
        if let Some(fixture) = resolve_fixture_wizard(&reference, resolver, wizard_mode)? {
            let source = if reference.starts_with("file://") {
                let local_path = reference.trim_start_matches("file://");
                let path = PathBuf::from(local_path);
                let (_abs_path, uri_path) = normalize_local_wasm_path(&path, flow_path)?;
                ComponentSourceRefV1::Local {
                    path: uri_path,
                    digest: None,
                }
            } else {
                classify_remote_source(&reference, None)
            };
            return Ok(WizardComponentResolution {
                wasm_bytes: Vec::new(),
                digest: None,
                source,
                fixture: Some(fixture),
            });
        }
        let resolved = resolve_ref_to_bytes(&reference, resolver)?;
        let source = if reference.starts_with("file://") {
            let local_path = reference.trim_start_matches("file://");
            let path = PathBuf::from(local_path);
            let (abs_path, uri_path) = normalize_local_wasm_path(&path, flow_path)?;
            let digest = Some(compute_local_digest(&abs_path)?);
            ComponentSourceRefV1::Local {
                path: uri_path,
                digest,
            }
        } else {
            classify_remote_source(&reference, resolved.digest.clone())
        };
        return Ok(WizardComponentResolution {
            wasm_bytes: resolved.bytes,
            digest: resolved.digest,
            source,
            fixture: None,
        });
    }

    anyhow::bail!(
        "component source is required; provide --local-wasm, --component <ref>, or component_id"
    );
}

struct ResolvedRefBytes {
    bytes: Vec<u8>,
    digest: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct FixtureIndex {
    components: std::collections::BTreeMap<String, FixtureComponentEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct FixtureComponentEntry {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    abi_version: Option<String>,
}

fn fixture_key(reference: &str) -> String {
    reference
        .trim_start_matches("oci://")
        .trim_start_matches("repo://")
        .trim_start_matches("store://")
        .trim_start_matches("file://")
        .replace(['/', ':', '@'], "_")
}

fn strip_reference_scheme(reference: &str) -> &str {
    reference
        .strip_prefix("oci://")
        .or_else(|| reference.strip_prefix("repo://"))
        .or_else(|| reference.strip_prefix("store://"))
        .or_else(|| reference.strip_prefix("file://"))
        .unwrap_or(reference)
}

fn load_fixture_index(root: &Path) -> Result<Option<FixtureIndex>> {
    let path = root.join("index.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("read fixture index {}", path.display()))?;
    let parsed: FixtureIndex = serde_json::from_str(&text).context("parse fixture index JSON")?;
    Ok(Some(parsed))
}

fn fixture_entry_for_reference<'a>(
    index: &'a FixtureIndex,
    reference: &str,
) -> Option<&'a FixtureComponentEntry> {
    if let Some(entry) = index.components.get(reference) {
        return Some(entry);
    }
    let stripped = strip_reference_scheme(reference);
    index.components.get(stripped)
}

fn fixture_component_dir(
    root: &Path,
    reference: &str,
    entry: Option<&FixtureComponentEntry>,
) -> PathBuf {
    if let Some(entry) = entry
        && let Some(path) = entry.path.as_ref()
    {
        return root.join(path);
    }
    root.join("components").join(fixture_key(reference))
}

fn resolve_ref_to_bytes(reference: &str, resolver: Option<&String>) -> Result<ResolvedRefBytes> {
    if let Some(resolver) = resolver
        && let Some(root) = resolver.strip_prefix("fixture://")
    {
        return resolve_fixture_bytes(reference, Path::new(root));
    }

    let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let client = DistClient::new(Default::default());
    let resolved = rt
        .block_on(client.resolve_ref(reference))
        .map_err(|e| anyhow::anyhow!("resolve reference {reference}: {e}"))?;
    let path = resolved
        .cache_path
        .ok_or_else(|| anyhow::anyhow!("resolved reference {reference} without cache path"))?;
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(ResolvedRefBytes {
        bytes,
        digest: Some(resolved.digest),
    })
}

fn resolve_fixture_bytes(reference: &str, root: &Path) -> Result<ResolvedRefBytes> {
    let index = load_fixture_index(root)?;
    if let Some(index) = index
        && let Some(entry) = fixture_entry_for_reference(&index, reference)
    {
        let dir = fixture_component_dir(root, reference, Some(entry));
        let wasm_path = dir.join("component.wasm");
        if !wasm_path.exists() {
            anyhow::bail!(
                "fixture resolver missing wasm for {} (expected {})",
                reference,
                wasm_path.display()
            );
        }
        let bytes =
            fs::read(&wasm_path).with_context(|| format!("read {}", wasm_path.display()))?;
        let digest = Some(compute_local_digest(&wasm_path)?);
        return Ok(ResolvedRefBytes { bytes, digest });
    }

    let key = fixture_key(reference);
    let direct = root.join(format!("{key}.wasm"));
    let nested = root.join(&key).join("component.wasm");
    let path = if direct.exists() { &direct } else { &nested };
    if !path.exists() {
        anyhow::bail!(
            "fixture resolver missing {} (looked for {} or {})",
            reference,
            direct.display(),
            nested.display()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let digest = Some(compute_local_digest(path)?);
    Ok(ResolvedRefBytes { bytes, digest })
}

fn resolve_fixture_wizard(
    reference: &str,
    resolver: Option<&String>,
    wizard_mode: wizard_ops::WizardMode,
) -> Result<Option<WizardFixture>> {
    let Some(resolver) = resolver else {
        return Ok(None);
    };
    let Some(root) = resolver.strip_prefix("fixture://") else {
        return Ok(None);
    };
    let root = Path::new(root);
    let mode = wizard_mode.as_str();
    let legacy_mode = wizard_mode_legacy_label(wizard_mode);
    if let Some(index) = load_fixture_index(root)?
        && let Some(entry) = fixture_entry_for_reference(&index, reference)
    {
        let dir = fixture_component_dir(root, reference, Some(entry));
        let describe_path = dir.join("describe.cbor");
        let qa_spec_path = {
            let path = dir.join(format!("qa_{mode}.cbor"));
            if path.exists() {
                path
            } else if let Some(legacy) = legacy_mode {
                dir.join(format!("qa_{legacy}.cbor"))
            } else {
                path
            }
        };
        let apply_path = {
            let path = dir.join(format!("apply_{mode}_config.cbor"));
            if path.exists() {
                path
            } else if let Some(legacy) = legacy_mode {
                dir.join(format!("apply_{legacy}_config.cbor"))
            } else {
                path
            }
        };
        if !qa_spec_path.exists() || !apply_path.exists() {
            anyhow::bail!(
                "fixture wizard missing qa/apply for {} (expected {} and {})",
                reference,
                qa_spec_path.display(),
                apply_path.display()
            );
        }
        if !describe_path.exists() {
            anyhow::bail!(
                "fixture wizard missing describe for {} (expected {})",
                reference,
                describe_path.display()
            );
        }
        let qa_spec_cbor =
            fs::read(&qa_spec_path).with_context(|| format!("read {}", qa_spec_path.display()))?;
        let apply_answers_cbor =
            fs::read(&apply_path).with_context(|| format!("read {}", apply_path.display()))?;
        let describe_cbor = fs::read(&describe_path)
            .with_context(|| format!("read {}", describe_path.display()))?;
        let abi = match entry.abi_version.as_deref() {
            Some("0.5.0") => wizard_ops::WizardAbi::Legacy,
            Some(_) => wizard_ops::WizardAbi::V6,
            None => wizard_ops::WizardAbi::V6,
        };
        return Ok(Some(WizardFixture {
            abi,
            describe_cbor,
            qa_spec_cbor,
            apply_answers_cbor,
        }));
    }

    let key = fixture_key(reference);
    let mode_path = root.join(format!("{key}.qa-{mode}.cbor"));
    let mode_apply = root.join(format!("{key}.apply-{mode}-config.cbor"));
    let legacy_mode_path = legacy_mode.map(|legacy| root.join(format!("{key}.qa-{legacy}.cbor")));
    let legacy_mode_apply =
        legacy_mode.map(|legacy| root.join(format!("{key}.apply-{legacy}-config.cbor")));
    let qa_spec_path = if mode_path.exists() {
        mode_path
    } else if let Some(path) = legacy_mode_path
        && path.exists()
    {
        path
    } else {
        root.join(format!("{key}.qa-spec.cbor"))
    };
    let apply_path = if mode_apply.exists() {
        mode_apply
    } else if let Some(path) = legacy_mode_apply
        && path.exists()
    {
        path
    } else {
        root.join(format!("{key}.apply-answers.cbor"))
    };
    let describe_path = root.join(format!("{key}.describe.cbor"));
    let abi_path = root.join(format!("{key}.abi"));

    if !qa_spec_path.exists()
        && !apply_path.exists()
        && !describe_path.exists()
        && !abi_path.exists()
    {
        return Ok(None);
    }
    if !qa_spec_path.exists() || !apply_path.exists() {
        anyhow::bail!(
            "fixture wizard missing qa-spec/apply-answers for {} (expected {} and {})",
            reference,
            qa_spec_path.display(),
            apply_path.display()
        );
    }
    let qa_spec_cbor =
        fs::read(&qa_spec_path).with_context(|| format!("read {}", qa_spec_path.display()))?;
    let apply_answers_cbor =
        fs::read(&apply_path).with_context(|| format!("read {}", apply_path.display()))?;
    let describe_cbor = if describe_path.exists() {
        fs::read(&describe_path).with_context(|| format!("read {}", describe_path.display()))?
    } else {
        Vec::new()
    };
    let abi = if abi_path.exists() {
        let text = fs::read_to_string(&abi_path)
            .with_context(|| format!("read {}", abi_path.display()))?;
        match text.trim() {
            "0.5.0" => wizard_ops::WizardAbi::Legacy,
            _ => wizard_ops::WizardAbi::V6,
        }
    } else {
        wizard_ops::WizardAbi::V6
    };

    Ok(Some(WizardFixture {
        abi,
        describe_cbor,
        qa_spec_cbor,
        apply_answers_cbor,
    }))
}

fn resolve_component_id_reference(
    component_id: &str,
    distributor_url: Option<&String>,
    auth_token: Option<&String>,
    tenant: Option<&String>,
    env: Option<&String>,
    pack: Option<&String>,
    component_version: Option<&String>,
) -> Result<String> {
    let base_url = distributor_url.ok_or_else(|| {
        anyhow::anyhow!("--distributor-url is required for component_id resolution")
    })?;
    let tenant = tenant
        .ok_or_else(|| anyhow::anyhow!("--tenant is required for component_id resolution"))?;
    let env =
        env.ok_or_else(|| anyhow::anyhow!("--env is required for component_id resolution"))?;
    let pack =
        pack.ok_or_else(|| anyhow::anyhow!("--pack is required for component_id resolution"))?;
    let version = component_version.ok_or_else(|| {
        anyhow::anyhow!("--component-version is required for component_id resolution")
    })?;

    let cfg = DistributorClientConfig {
        base_url: Some(base_url.to_string()),
        environment_id: DistributorEnvironmentId::from(env.as_str()),
        tenant: TenantCtx::new(
            EnvId::try_from(env.as_str()).map_err(|e| anyhow::anyhow!("env id: {e}"))?,
            TenantId::try_from(tenant.as_str()).map_err(|e| anyhow::anyhow!("tenant id: {e}"))?,
        ),
        auth_token: auth_token.cloned(),
        extra_headers: None,
        request_timeout: None,
    };
    let client = HttpDistributorClient::new(cfg)
        .map_err(|err| anyhow::anyhow!("init distributor client: {err}"))?;
    let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let resp = rt
        .block_on(
            client.resolve_component(ResolveComponentRequest {
                tenant: TenantCtx::new(
                    EnvId::try_from(env.as_str()).map_err(|e| anyhow::anyhow!("env id: {e}"))?,
                    TenantId::try_from(tenant.as_str())
                        .map_err(|e| anyhow::anyhow!("tenant id: {e}"))?,
                ),
                environment_id: DistributorEnvironmentId::from(env.as_str()),
                pack_id: pack.to_string(),
                component_id: component_id.to_string(),
                version: version.to_string(),
                extra: serde_json::Value::Object(Default::default()),
            }),
        )
        .map_err(|err| anyhow::anyhow!("resolve component via distributor: {err}"))?;

    match resp.artifact {
        greentic_types::ArtifactLocation::FilePath { path } => Ok(format!("file://{path}")),
        greentic_types::ArtifactLocation::OciReference { reference } => Ok(reference),
        greentic_types::ArtifactLocation::DistributorInternal { handle } => Err(anyhow!(
            "distributor returned internal handle {handle}; cannot resolve artifact"
        )),
    }
}

fn ensure_sidecar_source_available(source: &ComponentSourceRefV1, flow_path: &Path) -> Result<()> {
    match source {
        ComponentSourceRefV1::Local { path, .. } => {
            let abs = local_path_from_sidecar(path, flow_path);
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

fn resolve_component_manifest_path(
    source: &ComponentSourceRefV1,
    flow_path: &Path,
) -> Result<PathBuf> {
    let manifest_path = match source {
        ComponentSourceRefV1::Local { path, .. } => local_path_from_sidecar(path, flow_path)
            .parent()
            .map(|p| p.join("component.manifest.json"))
            .unwrap_or_else(|| {
                flow_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("component.manifest.json")
            }),
        ComponentSourceRefV1::Oci { r#ref, digest } => {
            let client = DistClient::new(Default::default());
            let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
            let cached = if let Some(d) = digest {
                rt.block_on(client.fetch_digest(d))
            } else {
                rt.block_on(client.ensure_cached(r#ref))
                    .map(|r| r.cache_path.unwrap_or_default())
            };
            let mut candidate = cached
                .ok()
                .and_then(|artifact| artifact.parent().map(|p| p.join("component.manifest.json")))
                .unwrap_or_else(|| PathBuf::from("component.manifest.json"));
            if candidate.exists() {
                return Ok(candidate);
            }
            let resolved_ref = if let Some(d) = digest {
                if r#ref.contains('@') {
                    r#ref.to_string()
                } else {
                    format!("{}@{}", r#ref, d)
                }
            } else {
                r#ref.to_string()
            };
            let resolved = rt
                .block_on(client.resolve_ref(&resolved_ref))
                .map_err(|e| anyhow::anyhow!("resolve component {}: {e}", resolved_ref))?;
            if let Some(path) = resolved.cache_path
                && let Some(parent) = path.parent()
            {
                candidate = parent.join("component.manifest.json");
            }
            candidate
        }
        ComponentSourceRefV1::Repo { r#ref, digest }
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
        anyhow::bail!(
            "component.manifest.json not found at {}",
            manifest_path.display()
        );
    }
    Ok(manifest_path)
}

fn load_component_payload(
    source: &ComponentSourceRefV1,
    flow_path: &Path,
) -> Result<Option<serde_json::Value>> {
    ensure_sidecar_source_available(source, flow_path)?;
    let manifest_path = match source {
        ComponentSourceRefV1::Local { path, .. } => local_path_from_sidecar(path, flow_path)
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
