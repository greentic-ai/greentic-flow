use anyhow::{Context, Result, anyhow};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use include_dir::{Dir, include_dir};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const EMBEDDED_FLOW_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schemas/ygtc.flow.schema.json"
));
const EMBEDDED_I18N_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/i18n");
const EMBEDDED_WIZARD_I18N_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/i18n/wizard");

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
    i18n::{I18nCatalog, resolve_cli_text, resolve_locale},
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
use greentic_qa_lib::{
    I18nConfig as QaI18nConfig, WizardDriver, WizardFrontend, WizardRunConfig as QaWizardRunConfig,
};
use greentic_types::flow_resolve::{
    ComponentSourceRefV1, FLOW_RESOLVE_SCHEMA_VERSION, FlowResolveV1, NodeResolveV1, ResolveModeV1,
    read_flow_resolve, sidecar_path_for_flow, write_flow_resolve,
};
use greentic_types::schemas::component::v0_6_0::{ComponentQaSpec, QuestionKind};
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

fn hash_schema_source(
    hasher: &mut Sha256,
    source: &greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SchemaSource,
) {
    match source {
        greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SchemaSource::CborSchemaId(id) => {
            hasher.update([0]);
            hasher.update(id.as_bytes());
        }
        greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SchemaSource::InlineCbor(bytes) => {
            hasher.update([1]);
            hasher.update(bytes);
        }
        greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SchemaSource::RefPackPath(path) => {
            hasher.update([2]);
            hasher.update(path.as_bytes());
        }
        greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SchemaSource::RefUri(uri) => {
            hasher.update([3]);
            hasher.update(uri.as_bytes());
        }
    }
}

fn hash_io_schema(
    hasher: &mut Sha256,
    schema: &greentic_interfaces_host::component_v0_6::exports::greentic::component::node::IoSchema,
) {
    hash_schema_source(hasher, &schema.schema);
    hasher.update(schema.content_type.as_bytes());
    if let Some(version) = &schema.schema_version {
        hasher.update(version.as_bytes());
    }
}

fn canonical_descriptor_hash(
    descriptor: &greentic_interfaces_host::component_v0_6::exports::greentic::component::node::ComponentDescriptor,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(descriptor.name.as_bytes());
    hasher.update(descriptor.version.as_bytes());
    if let Some(summary) = &descriptor.summary {
        hasher.update(summary.as_bytes());
    }
    for capability in &descriptor.capabilities {
        hasher.update(capability.as_bytes());
    }
    for op in &descriptor.ops {
        hasher.update(op.name.as_bytes());
        if let Some(summary) = &op.summary {
            hasher.update(summary.as_bytes());
        }
        hash_io_schema(&mut hasher, &op.input);
        hash_io_schema(&mut hasher, &op.output);
        for example in &op.examples {
            hasher.update(example.title.as_bytes());
            hasher.update(&example.input_cbor);
            hasher.update(&example.output_cbor);
        }
    }
    for schema in &descriptor.schemas {
        hasher.update(schema.id.as_bytes());
        hasher.update(schema.content_type.as_bytes());
        hasher.update(schema.blake3_hash.as_bytes());
        hasher.update(schema.version.as_bytes());
        if let Some(bytes) = &schema.bytes {
            hasher.update(bytes);
        }
        if let Some(uri) = &schema.uri {
            hasher.update(uri.as_bytes());
        }
    }
    if let Some(setup) = &descriptor.setup {
        hash_schema_source(&mut hasher, &setup.qa_spec);
        hash_schema_source(&mut hasher, &setup.answers_schema);
        for example in &setup.examples {
            hasher.update(example.title.as_bytes());
            hasher.update(&example.answers_cbor);
        }
        for output in &setup.outputs {
            match output {
                greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SetupOutput::ConfigOnly => {
                    hasher.update([4]);
                }
                greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SetupOutput::TemplateScaffold(scaffold) => {
                    hasher.update([5]);
                    hasher.update(scaffold.template_ref.as_bytes());
                    if let Some(layout) = &scaffold.output_layout {
                        hasher.update(layout.as_bytes());
                    }
                }
            }
        }
    }
    format!("{:x}", hasher.finalize())
}

fn derive_contract_meta_from_descriptor(
    descriptor: &greentic_interfaces_host::component_v0_6::exports::greentic::component::node::ComponentDescriptor,
    operation_id: &str,
) -> Result<(
    Option<greentic_types::schemas::common::schema_ir::SchemaIr>,
    flow_meta::ComponentContractMeta,
)> {
    let op = descriptor
        .ops
        .iter()
        .find(|op| op.name == operation_id)
        .ok_or_else(|| anyhow!("operation '{}' not found in descriptor.ops", operation_id))?;

    let mut schema_hasher = Sha256::new();
    schema_hasher.update(op.name.as_bytes());
    hash_io_schema(&mut schema_hasher, &op.input);
    hash_io_schema(&mut schema_hasher, &op.output);
    let schema_hash = format!("{:x}", schema_hasher.finalize());

    let (config_schema, config_schema_cbor) = match &op.input.schema {
        greentic_interfaces_host::component_v0_6::exports::greentic::component::node::SchemaSource::InlineCbor(bytes) => {
            let schema = greentic_types::cbor::canonical::from_cbor::<
                greentic_types::schemas::common::schema_ir::SchemaIr,
            >(bytes)
            .map_err(|err| anyhow!("decode descriptor input schema cbor: {err}"))?;
            (Some(schema), Some(bytes_to_hex(bytes)))
        }
        _ => (None, None),
    };

    let meta = flow_meta::ComponentContractMeta {
        describe_hash: canonical_descriptor_hash(descriptor),
        operation_id: operation_id.to_string(),
        schema_hash,
        component_version: Some(descriptor.version.clone()),
        world: Some("greentic:component@0.6.0".to_string()),
        config_schema_cbor,
    };
    Ok((config_schema, meta))
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
    let mut catalog = I18nCatalog::default();
    merge_i18n_json_embedded(&mut catalog, "en");
    merge_i18n_json_embedded(&mut catalog, &locale);
    if let Some((language, _)) = locale.split_once('-')
        && !language.is_empty()
        && language != locale
    {
        merge_i18n_json_embedded(&mut catalog, language);
    }
    (catalog, locale)
}

fn merge_component_i18n_catalog(
    catalog: &mut I18nCatalog,
    locale: &str,
    flow_path: &Path,
    source: &ComponentSourceRefV1,
) {
    let Ok(manifest_path) = resolve_component_manifest_path(source, flow_path) else {
        return;
    };
    let Some(root) = manifest_path.parent() else {
        return;
    };
    for candidate in greentic_flow::i18n::locale_fallback_chain(locale) {
        for rel in ["i18n", "assets/i18n"] {
            let path = root.join(rel).join(format!("{candidate}.json"));
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            merge_i18n_json_str(catalog, &candidate, &text);
        }
    }
}

fn merge_i18n_json_embedded(catalog: &mut I18nCatalog, locale: &str) {
    let file_name = format!("{locale}.json");
    let Some(file) = EMBEDDED_I18N_DIR.get_file(&file_name) else {
        return;
    };
    let Some(text) = file.contents_utf8() else {
        return;
    };
    merge_i18n_json_str(catalog, locale, text);
}

fn merge_i18n_json_str(catalog: &mut I18nCatalog, locale: &str, text: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(entries) = value.as_object() else {
        return;
    };
    for (key, value) in entries {
        if let Some(message) = value.as_str() {
            catalog.insert(key.clone(), locale.to_string(), message.to_string());
        }
    }
}

fn cli_requested_locale() -> Option<String> {
    let mut args = env::args();
    while let Some(arg) = args.next() {
        if arg == "--locale" {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix("--locale=")
            && !value.trim().is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

fn localized_cli_command(catalog: &I18nCatalog, locale: &str) -> clap::Command {
    localize_help_tree(Cli::command(), catalog, locale, &[])
}

fn normalize_help_key_part(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch.to_ascii_lowercase(),
            _ => '_',
        })
        .collect()
}

fn help_path_key(path: &[String]) -> String {
    if path.is_empty() {
        "top".to_string()
    } else {
        path.iter()
            .map(|seg| normalize_help_key_part(seg))
            .collect::<Vec<_>>()
            .join(".")
    }
}

fn localize_help_tree(
    mut cmd: clap::Command,
    catalog: &I18nCatalog,
    locale: &str,
    path: &[String],
) -> clap::Command {
    let path_key = help_path_key(path);
    if let Some(about) = cmd.get_about().map(|v| v.to_string())
        && !about.trim().is_empty()
    {
        let key = format!("cli.help.command.{path_key}.about");
        cmd = cmd.about(resolve_cli_text(catalog, locale, &key, &about));
    }
    if let Some(long_about) = cmd.get_long_about().map(|v| v.to_string())
        && !long_about.trim().is_empty()
    {
        let key = format!("cli.help.command.{path_key}.long_about");
        cmd = cmd.long_about(resolve_cli_text(catalog, locale, &key, &long_about));
    }

    let arg_ids: Vec<String> = cmd
        .get_arguments()
        .map(|arg| arg.get_id().as_str().to_string())
        .collect();
    for arg_id in arg_ids {
        let arg_key = normalize_help_key_part(&arg_id);
        cmd = cmd.mut_arg(arg_id.as_str(), |mut arg| {
            if let Some(help) = arg.get_help().map(|v| v.to_string())
                && !help.trim().is_empty()
            {
                let key = format!("cli.help.arg.{path_key}.{arg_key}.help");
                arg = arg.help(resolve_cli_text(catalog, locale, &key, &help));
            }
            if let Some(long_help) = arg.get_long_help().map(|v| v.to_string())
                && !long_help.trim().is_empty()
            {
                let key = format!("cli.help.arg.{path_key}.{arg_key}.long_help");
                arg = arg.long_help(resolve_cli_text(catalog, locale, &key, &long_help));
            }
            arg
        });
    }

    let sub_names: Vec<String> = cmd
        .get_subcommands()
        .map(|sc| sc.get_name().to_string())
        .collect();
    for sub_name in sub_names {
        let mut sub_path = path.to_vec();
        sub_path.push(sub_name.clone());
        cmd = cmd.mut_subcommand(sub_name.as_str(), |sc| {
            localize_help_tree(sc, catalog, locale, &sub_path)
        });
    }
    cmd
}

fn collect_help_i18n_entries(
    cmd: &clap::Command,
    path: &[String],
    out: &mut BTreeMap<String, String>,
) {
    let path_key = help_path_key(path);
    if let Some(about) = cmd.get_about().map(|v| v.to_string())
        && !about.trim().is_empty()
    {
        out.insert(format!("cli.help.command.{path_key}.about"), about);
    }
    if let Some(long_about) = cmd.get_long_about().map(|v| v.to_string())
        && !long_about.trim().is_empty()
    {
        out.insert(
            format!("cli.help.command.{path_key}.long_about"),
            long_about,
        );
    }
    for arg in cmd.get_arguments() {
        let arg_key = normalize_help_key_part(arg.get_id().as_str());
        if let Some(help) = arg.get_help().map(|v| v.to_string())
            && !help.trim().is_empty()
        {
            out.insert(format!("cli.help.arg.{path_key}.{arg_key}.help"), help);
        }
        if let Some(long_help) = arg.get_long_help().map(|v| v.to_string())
            && !long_help.trim().is_empty()
        {
            out.insert(
                format!("cli.help.arg.{path_key}.{arg_key}.long_help"),
                long_help,
            );
        }
    }
    for sc in cmd.get_subcommands() {
        let mut sub_path = path.to_vec();
        sub_path.push(sc.get_name().to_string());
        collect_help_i18n_entries(sc, &sub_path, out);
    }
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
    /// Pack root directory.
    pack: PathBuf,
    /// Write wizard answers to a JSON file without prompting.
    #[arg(long = "answers-file")]
    answers_file: Option<PathBuf>,
    /// Validate and run doctor, but do not persist flow mutations.
    #[arg(long = "dry-run")]
    dry_run: bool,
}

#[derive(Debug, Clone, Default)]
struct WizardRunConfig {
    answers_file: Option<PathBuf>,
    dry_run: bool,
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
    if env::args().any(|arg| arg == "--dump-help-i18n") {
        let mut entries = BTreeMap::new();
        collect_help_i18n_entries(&Cli::command(), &[], &mut entries);
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    let requested_locale = cli_requested_locale();
    let (catalog, locale) = default_i18n_catalog(requested_locale.as_deref());
    let cmd = localized_cli_command(&catalog, &locale);
    let matches = match cmd.try_get_matches() {
        Ok(matches) => matches,
        Err(err) => err.exit(),
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    if let Some(locale) = cli.locale.as_deref()
        && !locale.trim().is_empty()
    {
        unsafe {
            std::env::set_var("GREENTIC_LOCALE", locale.trim());
        }
    }
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
        Commands::Wizard(args) => handle_wizard(args),
    }
}

fn handle_wizard(args: WizardArgs) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_wizard_menu_with_config(
        &args.pack,
        stdin,
        stdout,
        WizardRunConfig {
            answers_file: args.answers_file,
            dry_run: args.dry_run,
        },
    )
}

#[derive(Debug, Clone)]
enum WizardScreen {
    MainMenu,
    FlowSelect,
    FlowOps { flow_path: PathBuf },
}

#[derive(Debug)]
struct WizardSession {
    real_pack_dir: PathBuf,
    staged_pack_dir: PathBuf,
    dirty: bool,
    config: WizardRunConfig,
    answers_log: serde_json::Map<String, serde_json::Value>,
    answers_output_path: Option<PathBuf>,
}

#[cfg(test)]
fn run_wizard_menu_with_io<R: Read, W: Write>(
    pack_dir: &Path,
    mut reader: R,
    mut writer: W,
) -> Result<()> {
    run_wizard_menu_with_config(
        pack_dir,
        &mut reader,
        &mut writer,
        WizardRunConfig::default(),
    )
}

fn run_wizard_menu_with_config<R: Read, W: Write>(
    pack_dir: &Path,
    mut reader: R,
    mut writer: W,
    config: WizardRunConfig,
) -> Result<()> {
    let staged_pack_dir = create_wizard_staging_pack(pack_dir)?;
    let preloaded_answers = if let Some(path) = config.answers_file.as_ref() {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            pack_dir.join(path)
        };
        load_wizard_answers_file(&resolved)?
    } else {
        serde_json::Map::new()
    };
    let mut session = WizardSession {
        real_pack_dir: pack_dir.to_path_buf(),
        staged_pack_dir,
        dirty: false,
        config,
        answers_log: preloaded_answers,
        answers_output_path: None,
    };
    let mut screen = WizardScreen::MainMenu;
    loop {
        match screen.clone() {
            WizardScreen::MainMenu => {
                let answer = wizard_menu_answer(
                    &mut reader,
                    &mut writer,
                    "main.menu",
                    &wizard_t("wizard.menu.main.prompt"),
                    &["1", "2", "3", "4", "5", "0"],
                )?;
                session.answers_log.insert(
                    "main.menu".to_string(),
                    serde_json::Value::String(answer.clone()),
                );
                match answer.as_str() {
                    "1" => {
                        if let Err(err) = wizard_add_flow_with_io(
                            &session.staged_pack_dir,
                            &mut reader,
                            &mut writer,
                            &mut session.answers_log,
                        ) {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                        }
                    }
                    "2" => {
                        screen = WizardScreen::FlowSelect;
                    }
                    "3" => {
                        if let Err(err) = wizard_generate_translations_with_io(
                            &session.staged_pack_dir,
                            &mut reader,
                            &mut writer,
                            &mut session.answers_log,
                        ) {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                        }
                    }
                    "4" => {
                        if let Err(err) =
                            wizard_save_staged_changes(&mut session, &mut reader, &mut writer)
                        {
                            writeln!(writer, "{}", err).ok();
                        }
                    }
                    "5" => {
                        if let Err(err) =
                            wizard_export_answers_with_io(&mut session, &mut reader, &mut writer)
                        {
                            writeln!(writer, "{}", err).ok();
                        }
                    }
                    "0" => {
                        if session.dirty {
                            writeln!(writer, "{}", wizard_t("wizard.save.discarded")).ok();
                        }
                        let _ = fs::remove_dir_all(&session.staged_pack_dir);
                        return Ok(());
                    }
                    _ => {}
                }
            }
            WizardScreen::FlowSelect => {
                let flows = collect_pack_flows(&session.staged_pack_dir)?;
                let mut prompt = format!("{}\n", wizard_t("wizard.menu.flow_select.title"));
                for (idx, flow) in flows.iter().enumerate() {
                    let rel = flow.strip_prefix(&session.staged_pack_dir).unwrap_or(flow);
                    prompt.push_str(&format!("{}. {}\n", idx + 1, rel.display()));
                }
                prompt.push_str(&format!(
                    "{}\n{}",
                    wizard_t("wizard.menu.nav.back"),
                    wizard_t("wizard.menu.nav.main")
                ));
                let mut choices: Vec<String> = (1..=flows.len()).map(|n| n.to_string()).collect();
                choices.push("0".to_string());
                choices.push("M".to_string());
                let choice_refs: Vec<&str> = choices.iter().map(String::as_str).collect();
                let answer = wizard_menu_answer(
                    &mut reader,
                    &mut writer,
                    "flow.select",
                    &prompt,
                    &choice_refs,
                )?;
                session.answers_log.insert(
                    "flow.select".to_string(),
                    serde_json::Value::String(answer.clone()),
                );
                match answer.as_str() {
                    "0" => screen = WizardScreen::MainMenu,
                    "M" => screen = WizardScreen::MainMenu,
                    raw => {
                        let idx = raw
                            .parse::<usize>()
                            .context("parse selected flow index")?
                            .saturating_sub(1);
                        if let Some(flow_path) = flows.get(idx) {
                            screen = WizardScreen::FlowOps {
                                flow_path: flow_path.clone(),
                            };
                        }
                    }
                }
            }
            WizardScreen::FlowOps { flow_path } => {
                let rel = flow_path
                    .strip_prefix(&session.staged_pack_dir)
                    .unwrap_or(&flow_path);
                let prompt = wizard_t_with(
                    "wizard.menu.flow_ops.prompt",
                    &[("flow", &rel.display().to_string())],
                );
                let answer = wizard_menu_answer(
                    &mut reader,
                    &mut writer,
                    "flow.ops",
                    &prompt,
                    &["1", "2", "3", "4", "5", "6", "7", "0", "M"],
                )?;
                session.answers_log.insert(
                    "flow.ops".to_string(),
                    serde_json::Value::String(answer.clone()),
                );
                match answer.as_str() {
                    "0" => screen = WizardScreen::FlowSelect,
                    "M" => screen = WizardScreen::MainMenu,
                    "1" => {
                        if let Err(err) = wizard_edit_flow_summary_with_io(
                            &flow_path,
                            &mut reader,
                            &mut writer,
                            &mut session.answers_log,
                        ) {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                        }
                    }
                    "2" => {
                        if let Err(err) = wizard_list_steps_with_io(&flow_path, &mut writer) {
                            writeln!(writer, "{}", err).ok();
                        }
                    }
                    "3" => {
                        if let Err(err) = wizard_add_step_with_io(
                            &session.staged_pack_dir,
                            &flow_path,
                            &mut reader,
                            &mut writer,
                        ) {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                        }
                    }
                    "4" => {
                        if let Err(err) = wizard_update_step_with_io(
                            &session.staged_pack_dir,
                            &flow_path,
                            &mut reader,
                            &mut writer,
                        ) {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                        }
                    }
                    "5" => {
                        if let Err(err) =
                            wizard_delete_step_with_io(&flow_path, &mut reader, &mut writer)
                        {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                        }
                    }
                    "6" => {
                        if let Err(err) =
                            wizard_delete_flow_with_io(&flow_path, &mut reader, &mut writer)
                        {
                            writeln!(writer, "{}", err).ok();
                        } else {
                            session.dirty = true;
                            screen = WizardScreen::FlowSelect;
                        }
                    }
                    "7" => {
                        if let Err(err) =
                            wizard_save_staged_changes(&mut session, &mut reader, &mut writer)
                        {
                            writeln!(writer, "{}", err).ok();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn create_wizard_staging_pack(pack_dir: &Path) -> Result<PathBuf> {
    if !pack_dir.exists() {
        anyhow::bail!(
            "{}",
            wizard_t_with(
                "wizard.error.pack_dir_not_found",
                &[("path", &pack_dir.display().to_string())]
            )
        );
    }
    let unique = format!(
        "greentic-flow-wizard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let stage_root = env::temp_dir().join(unique);
    fs::create_dir_all(&stage_root)
        .with_context(|| format!("create directory {}", stage_root.display()))?;
    for entry in ["flows", "i18n", "components"] {
        let src = pack_dir.join(entry);
        let dst = stage_root.join(entry);
        if src.exists() {
            copy_dir_recursive(&src, &dst)?;
        }
    }
    Ok(stage_root)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        fs::copy(src, dst)
            .with_context(|| format!("copy file {} -> {}", src.display(), dst.display()))?;
        return Ok(());
    }
    fs::create_dir_all(dst).with_context(|| format!("create directory {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("read directory {}", src.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", src.display()))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copy file {} -> {}", src_path.display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}

fn wizard_save_staged_changes<R: Read, W: Write>(
    session: &mut WizardSession,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    if !session.dirty {
        writeln!(writer, "{}", wizard_t("wizard.save.no_changes")).ok();
        return Ok(());
    }

    if !session.answers_log.is_empty() {
        let answers_path = wizard_answers_output_path(session, reader, writer)?;
        write_wizard_answers_file(&answers_path, &session.answers_log)?;
    }

    let flows_target = session.staged_pack_dir.join("flows");
    if wizard_has_empty_flow_nodes(&flows_target)? {
        anyhow::bail!("{}", wizard_t("wizard.save.empty_flow"));
    }
    if let Err(err) = wizard_validate_flows(&flows_target) {
        let details = err.to_string();
        if details.contains("start_node_exists: invalid start node ''")
            || details.contains("NodeId must not be empty")
        {
            anyhow::bail!("{}", wizard_t("wizard.save.empty_flow"));
        }
        return Err(anyhow!(
            "{}: {details}",
            wizard_t("wizard.save.doctor_failed")
        ));
    }

    if !session.config.dry_run {
        sync_staged_pack_back(session)?;
        session.dirty = false;
        writeln!(writer, "{}", wizard_t("wizard.save.done")).ok();
    } else {
        session.dirty = false;
        writeln!(writer, "{}", wizard_t("wizard.save.dry_run_done")).ok();
    }
    Ok(())
}

fn wizard_has_empty_flow_nodes(root: &Path) -> Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("read directory {}", dir.display()))?
        {
            let entry = entry.with_context(|| format!("read entry under {}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("ygtc") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .with_context(|| format!("read flow file {}", path.display()))?;
            let doc: serde_yaml_bw::Value = serde_yaml_bw::from_str(&text)
                .with_context(|| format!("parse flow file {}", path.display()))?;
            let nodes_len = doc
                .as_mapping()
                .and_then(|map| {
                    map.get(serde_yaml_bw::Value::String(
                        "nodes".to_string(),
                        None::<String>,
                    ))
                })
                .and_then(serde_yaml_bw::Value::as_mapping)
                .map(|map| map.len())
                .unwrap_or(0);
            if nodes_len == 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn wizard_validate_flows(target: &Path) -> Result<()> {
    let schema_text = EMBEDDED_FLOW_SCHEMA.to_string();
    let schema_label = "embedded ygtc.flow.schema.json".to_string();
    let schema_path = PathBuf::from("schemas/ygtc.flow.schema.json");
    let lint_ctx = LintContext {
        schema_text: &schema_text,
        schema_label: &schema_label,
        schema_path: schema_path.as_path(),
        registry: None,
        schema_mode: SchemaMode::Strict,
    };
    let mut failures = 0usize;
    lint_path(target, &lint_ctx, false, &mut failures)?;
    if failures == 0 {
        Ok(())
    } else {
        anyhow::bail!("{failures} flow(s) failed validation");
    }
}

fn wizard_answers_output_path<R: Read, W: Write>(
    session: &mut WizardSession,
    _reader: &mut R,
    _writer: &mut W,
) -> Result<PathBuf> {
    if let Some(path) = session.answers_output_path.clone() {
        return Ok(path);
    }
    let path = session
        .config
        .answers_file
        .clone()
        .unwrap_or_else(|| PathBuf::from("./answers.json"));
    let resolved = if path.is_absolute() {
        path
    } else {
        session.real_pack_dir.join(path)
    };
    session.answers_output_path = Some(resolved.clone());
    Ok(resolved)
}

fn wizard_answers_output_path_interactive<R: Read, W: Write>(
    session: &mut WizardSession,
    reader: &mut R,
    writer: &mut W,
) -> Result<PathBuf> {
    let answers = run_questions_with_qa_lib_io(
        &[Question {
            id: "wizard.answers.path".to_string(),
            prompt: wizard_t("wizard.answers.path.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: false,
            default: Some(serde_json::Value::String("./answers.json".to_string())),
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        }],
        HashMap::new(),
        &mut *reader,
        &mut *writer,
    )?;
    let raw = answers
        .get("wizard.answers.path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("./answers.json")
        .trim();
    let selected = if raw.is_empty() {
        "./answers.json"
    } else {
        raw
    };
    session.answers_log.insert(
        "wizard.answers.path".to_string(),
        serde_json::Value::String(selected.to_string()),
    );
    let path = PathBuf::from(selected);
    let resolved = if path.is_absolute() {
        path
    } else {
        session.real_pack_dir.join(path)
    };
    session.answers_output_path = Some(resolved.clone());
    Ok(resolved)
}

fn wizard_export_answers_with_io<R: Read, W: Write>(
    session: &mut WizardSession,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let answers_path = wizard_answers_output_path_interactive(session, reader, writer)?;
    write_wizard_answers_file(&answers_path, &session.answers_log)?;
    writeln!(
        writer,
        "{}",
        wizard_t_with(
            "wizard.answers.path.saved",
            &[("path", &answers_path.display().to_string())]
        )
    )
    .ok();
    Ok(())
}

fn write_wizard_answers_file(
    path: &Path,
    answers_log: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let payload = serde_json::Value::Object(answers_log.clone());
    let text = serde_json::to_string_pretty(&payload).context("serialize wizard answers")?;
    fs::write(path, text).with_context(|| format!("write wizard answers {}", path.display()))?;
    Ok(())
}

fn load_wizard_answers_file(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read wizard answers {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse wizard answers {}", path.display()))?;
    Ok(value.as_object().cloned().unwrap_or_default())
}

fn sync_staged_pack_back(session: &mut WizardSession) -> Result<()> {
    sync_staged_dir(session, "flows")?;
    sync_staged_dir(session, "i18n")?;
    sync_staged_dir(session, "components")?;
    Ok(())
}

fn sync_staged_dir(session: &WizardSession, name: &str) -> Result<()> {
    let staged_dir = session.staged_pack_dir.join(name);
    let real_dir = session.real_pack_dir.join(name);
    if real_dir.exists() {
        fs::remove_dir_all(&real_dir)
            .with_context(|| format!("remove directory {}", real_dir.display()))?;
    }
    if staged_dir.exists() {
        copy_dir_recursive(&staged_dir, &real_dir)?;
    } else {
        fs::create_dir_all(&real_dir)
            .with_context(|| format!("create directory {}", real_dir.display()))?;
    }
    Ok(())
}

fn wizard_menu_answer<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    question_id: &str,
    prompt: &str,
    choices: &[&str],
) -> Result<String> {
    let locale = resolve_locale(None);
    let qa_i18n = wizard_qa_i18n_config_for_locale(&locale);
    let spec = serde_json::json!({
        "id": format!("wizard.{question_id}"),
        "title": "wizard",
        "version": "1.0.0",
        "presentation": { "default_locale": locale },
        "questions": [{
            "id": question_id,
            "type": "enum",
            "title": prompt,
            "required": true,
            "choices": choices,
        }]
    });

    let mut driver = WizardDriver::new(QaWizardRunConfig {
        spec_json: spec.to_string(),
        initial_answers_json: None,
        frontend: WizardFrontend::JsonUi,
        i18n: qa_i18n,
        verbose: false,
    })
    .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
    loop {
        let ui_raw = driver
            .next_payload_json()
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
        if driver.is_complete() {
            break;
        }
        let ui: serde_json::Value = serde_json::from_str(&ui_raw)
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
        let next_id = ui
            .get("next_question_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.qa_runner_failed")))?;
        let question = ui
            .get("questions")
            .and_then(serde_json::Value::as_array)
            .and_then(|questions| {
                questions.iter().find(|q| {
                    q.get("id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|id| id == next_id)
                })
            })
            .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.qa_runner_failed")))?;
        let title = question
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(prompt);
        writeln!(writer, "{title}").ok();
        let valid_choices: Vec<String> = question
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        loop {
            write!(writer, "> ").ok();
            writer.flush().ok();
            let line = read_input_line(reader)?;
            if valid_choices.iter().any(|choice| choice == &line) {
                let patch = serde_json::json!({ next_id: line });
                driver
                    .submit_patch_json(&patch.to_string())
                    .map_err(|err| {
                        anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed"))
                    })?;
                break;
            }
            writeln!(
                writer,
                "{}",
                wizard_t_with(
                    "wizard.error.invalid_choice",
                    &[("choices", &valid_choices.join(", "))]
                )
            )
            .ok();
        }
    }
    let run = driver
        .finish()
        .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
    let value = run
        .answer_set
        .answers
        .get(question_id)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow!(
                "{}",
                wizard_t_with(
                    "wizard.error.missing_answer_for_question",
                    &[("id", question_id)]
                )
            )
        })?;
    Ok(value.to_string())
}

fn read_input_line<R: Read + ?Sized>(reader: &mut R) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            buf.push(byte[0]);
        }
    }
    Ok(String::from_utf8(buf)
        .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.invalid_utf8_input")))?
        .trim()
        .to_string())
}

fn run_questions_with_qa_lib_io<R: Read, W: Write>(
    questions: &[Question],
    seed: HashMap<String, serde_json::Value>,
    reader: &mut R,
    writer: &mut W,
) -> Result<HashMap<String, serde_json::Value>> {
    let locale = resolve_locale(None);
    let qa_i18n = wizard_qa_i18n_config_for_locale(&locale);
    let spec = qa_form_from_questions(questions)?;
    let mut driver = WizardDriver::new(QaWizardRunConfig {
        spec_json: spec.to_string(),
        initial_answers_json: Some(serde_json::Value::Object(map_from_answers(&seed)).to_string()),
        frontend: WizardFrontend::JsonUi,
        i18n: qa_i18n,
        verbose: false,
    })
    .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;

    while !driver.is_complete() {
        let payload_raw = driver
            .next_payload_json()
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
        let payload: serde_json::Value = serde_json::from_str(&payload_raw)
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
        let Some(question_id) = payload.get("next_question_id").and_then(|v| v.as_str()) else {
            break;
        };
        let question = payload
            .get("questions")
            .and_then(|v| v.as_array())
            .and_then(|questions| {
                questions.iter().find(|question| {
                    question
                        .get("id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| id == question_id)
                })
            })
            .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.qa_runner_failed")))?;
        let title = question
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(question_id);
        writeln!(writer, "{title}").ok();
        write!(writer, "> ").ok();
        writer.flush().ok();
        let line = read_input_line(reader)?;
        let answer = parse_qa_input_value(question, &line)?;
        let patch = serde_json::json!({ question_id: answer });
        driver
            .submit_patch_json(&patch.to_string())
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
    }

    let result = driver
        .finish()
        .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
    let Some(answers) = result.answer_set.answers.as_object() else {
        return Ok(seed);
    };
    Ok(answers
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn qa_form_from_questions(questions: &[Question]) -> Result<serde_json::Value> {
    let mut mapped = Vec::with_capacity(questions.len());
    for question in questions {
        let qtype = match question.kind {
            greentic_flow::questions::QuestionKind::String => "string",
            greentic_flow::questions::QuestionKind::Bool => "boolean",
            greentic_flow::questions::QuestionKind::Choice => "enum",
            greentic_flow::questions::QuestionKind::Int => "integer",
            greentic_flow::questions::QuestionKind::Float => "number",
        };
        let mut entry = serde_json::Map::new();
        entry.insert(
            "id".to_string(),
            serde_json::Value::String(question.id.clone()),
        );
        entry.insert(
            "type".to_string(),
            serde_json::Value::String(qtype.to_string()),
        );
        entry.insert(
            "title".to_string(),
            serde_json::Value::String(question.prompt.clone()),
        );
        entry.insert(
            "required".to_string(),
            serde_json::Value::Bool(question.required),
        );
        if let Some(default) = question.default.as_ref() {
            entry.insert(
                "default_value".to_string(),
                serde_json::Value::String(match default {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                }),
            );
        }
        if matches!(
            question.kind,
            greentic_flow::questions::QuestionKind::Choice
        ) && !question.choices.is_empty()
        {
            let choices = question
                .choices
                .iter()
                .map(|value| match value {
                    serde_json::Value::String(text) => serde_json::Value::String(text.clone()),
                    other => serde_json::Value::String(other.to_string()),
                })
                .collect::<Vec<_>>();
            entry.insert("choices".to_string(), serde_json::Value::Array(choices));
        }
        if let Some(show_if) = question.show_if.as_ref()
            && let Some(expr) = qa_visible_if_expr(show_if)
        {
            entry.insert("visible_if".to_string(), expr);
        }
        mapped.push(serde_json::Value::Object(entry));
    }

    Ok(serde_json::json!({
        "id": "wizard.form",
        "title": "wizard",
        "version": "1.0.0",
        "questions": mapped,
    }))
}

fn qa_visible_if_expr(show_if: &serde_json::Value) -> Option<serde_json::Value> {
    let id = show_if.get("id")?.as_str()?;
    let equals = show_if.get("equals")?.clone();
    Some(serde_json::json!({
        "op": "and",
        "expressions": [
            { "op": "is_set", "path": format!("/{id}") },
            {
                "op": "eq",
                "left": { "op": "answer", "path": format!("/{id}") },
                "right": { "op": "literal", "value": equals }
            }
        ]
    }))
}

fn parse_qa_input_value(question: &serde_json::Value, raw: &str) -> Result<serde_json::Value> {
    let qtype = question
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("string");
    let required = question
        .get("required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let trimmed = raw.trim();
    let effective = if trimmed.is_empty() {
        if let Some(default) = question.get("default").and_then(serde_json::Value::as_str) {
            default.to_string()
        } else if let Some(default) = question
            .get("default_value")
            .and_then(serde_json::Value::as_str)
        {
            default.to_string()
        } else if required {
            anyhow::bail!("{}", wizard_t("wizard.error.required_input"));
        } else {
            match qtype {
                "boolean" => "false".to_string(),
                "integer" => "0".to_string(),
                "number" => "0".to_string(),
                "enum" => question
                    .get("choices")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|values| values.first())
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                _ => String::new(),
            }
        }
    } else {
        trimmed.to_string()
    };

    match qtype {
        "boolean" => {
            let lower = effective.to_ascii_lowercase();
            Ok(serde_json::Value::Bool(matches!(
                lower.as_str(),
                "true" | "t" | "yes" | "y" | "1"
            )))
        }
        "integer" => {
            let value = effective.parse::<i64>().with_context(|| {
                wizard_t_with("wizard.error.invalid_integer", &[("value", &effective)])
            })?;
            Ok(serde_json::Value::Number(value.into()))
        }
        "number" => {
            let value = effective.parse::<f64>().with_context(|| {
                wizard_t_with("wizard.error.invalid_number", &[("value", &effective)])
            })?;
            let Some(number) = serde_json::Number::from_f64(value) else {
                anyhow::bail!("{}", wizard_t("wizard.error.number_out_of_range"));
            };
            Ok(serde_json::Value::Number(number))
        }
        "enum" => {
            let choices = question
                .get("choices")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.enum_choices_missing")))?;
            if let Ok(index) = effective.parse::<usize>()
                && index > 0
                && index <= choices.len()
                && let Some(value) = choices[index - 1].as_str()
            {
                return Ok(serde_json::Value::String(value.to_string()));
            }
            if choices
                .iter()
                .any(|choice| choice.as_str().is_some_and(|value| value == effective))
            {
                return Ok(serde_json::Value::String(effective));
            }
            anyhow::bail!(
                "{}",
                wizard_t_with(
                    "wizard.error.invalid_choice",
                    &[(
                        "choices",
                        &choices
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )]
                )
            );
        }
        _ => Ok(serde_json::Value::String(effective)),
    }
}

struct QaInteractiveIo<'a> {
    reader: &'a mut dyn Read,
    writer: &'a mut dyn Write,
}

fn run_component_qa_with_qa_lib(
    spec: &ComponentQaSpec,
    catalog: &I18nCatalog,
    locale: &str,
    mut answers: HashMap<String, serde_json::Value>,
    interactive: bool,
    mut qa_io: Option<&mut QaInteractiveIo<'_>>,
) -> Result<HashMap<String, serde_json::Value>> {
    let form_json = component_spec_to_qa_form_json(spec, catalog, locale)?;
    if !interactive {
        let form: qa_spec::FormSpec = serde_json::from_str(&form_json).context("parse qa form")?;
        let result = qa_spec::validate(
            &form,
            &serde_json::Value::Object(map_from_answers(&answers)),
        );
        if !result.valid {
            if !result.missing_required.is_empty() {
                anyhow::bail!(
                    "missing required answers: {} (provide --answers/--answers-file)",
                    result.missing_required.join(", ")
                );
            }
            let details = result
                .errors
                .iter()
                .map(|err| err.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("answers failed validation: {details}");
        }
        return Ok(answers);
    }

    let mut driver = WizardDriver::new(QaWizardRunConfig {
        spec_json: form_json,
        initial_answers_json: Some(
            serde_json::Value::Object(map_from_answers(&answers)).to_string(),
        ),
        frontend: WizardFrontend::JsonUi,
        i18n: QaI18nConfig {
            locale: Some(locale.to_string()),
            resolved: None,
            debug: false,
        },
        verbose: false,
    })
    .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;

    while !driver.is_complete() {
        let payload_raw = driver
            .next_payload_json()
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
        let payload: serde_json::Value = serde_json::from_str(&payload_raw)
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
        let Some(next_question_id) = payload.get("next_question_id").and_then(|v| v.as_str())
        else {
            break;
        };
        let question = payload
            .get("questions")
            .and_then(|v| v.as_array())
            .and_then(|questions| {
                questions.iter().find(|question| {
                    question
                        .get("id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| id == next_question_id)
                })
            })
            .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.qa_runner_failed")))?;

        let title = question
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(next_question_id);
        if let Some(io) = qa_io.as_deref_mut() {
            if let Some(description) = question.get("description").and_then(|v| v.as_str()) {
                writeln!(io.writer, "{title} ({description})").ok();
            } else {
                writeln!(io.writer, "{title}").ok();
            }
            if let Some(choices) = question.get("choices").and_then(|v| v.as_array()) {
                for (idx, choice) in choices.iter().enumerate() {
                    if let Some(value) = choice.as_str() {
                        writeln!(io.writer, "  {}. {}", idx + 1, value).ok();
                    }
                }
            }
        } else {
            if let Some(description) = question.get("description").and_then(|v| v.as_str()) {
                println!("{title} ({description})");
            } else {
                println!("{title}");
            }
            if let Some(choices) = question.get("choices").and_then(|v| v.as_array()) {
                for (idx, choice) in choices.iter().enumerate() {
                    if let Some(value) = choice.as_str() {
                        println!("  {}. {}", idx + 1, value);
                    }
                }
            }
        }

        let prompt = match question.get("type").and_then(|v| v.as_str()) {
            Some("enum") => wizard_t("wizard.qa.prompt.select_option"),
            Some("boolean") => wizard_t("wizard.qa.prompt.enter_true_false"),
            Some("number") => wizard_t("wizard.qa.prompt.enter_number"),
            Some("integer") => wizard_t("wizard.qa.prompt.enter_integer"),
            _ => wizard_t("wizard.qa.prompt.enter_text"),
        };
        let raw_owned = if let Some(io) = qa_io.as_deref_mut() {
            write!(io.writer, "{prompt}: ").ok();
            io.writer.flush().ok();
            read_input_line(io.reader)?
        } else {
            print!("{prompt}: ");
            io::stdout().flush().context("flush stdout")?;
            let mut line = String::new();
            io::stdin()
                .read_line(&mut line)
                .context("read interactive answer")?;
            line.trim().to_string()
        };
        let raw = raw_owned.as_str();
        let answer = parse_component_qa_input(question, raw)?;
        let patch = serde_json::json!({ next_question_id: answer });
        driver
            .submit_patch_json(&patch.to_string())
            .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
    }

    let result = driver
        .finish()
        .map_err(|err| anyhow!("{}: {err}", wizard_t("wizard.error.qa_runner_failed")))?;
    if let Some(object) = result.answer_set.answers.as_object() {
        answers = object.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    }
    Ok(answers)
}

fn qa_key_fallback_label(key: &str) -> String {
    let parts: Vec<&str> = key.split('.').collect();
    let token = if parts.len() >= 2 {
        match parts.last().copied().unwrap_or_default() {
            "label" | "title" | "description" | "help" => {
                parts.get(parts.len() - 2).copied().unwrap_or(key)
            }
            _ => parts.last().copied().unwrap_or(key),
        }
    } else {
        key
    };
    token
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_uppercase().collect::<String>();
                    out.push_str(chars.as_str());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_component_text(
    text: &greentic_types::i18n_text::I18nText,
    catalog: &I18nCatalog,
    locale: &str,
) -> String {
    let resolved = greentic_flow::i18n::resolve_text(text, catalog, locale);
    if resolved == text.key && text.key.starts_with("qa.") {
        return qa_key_fallback_label(&text.key);
    }
    resolved
}

fn component_spec_to_qa_form_json(
    spec: &ComponentQaSpec,
    catalog: &I18nCatalog,
    locale: &str,
) -> Result<String> {
    let mut questions = Vec::with_capacity(spec.questions.len());
    for question in &spec.questions {
        let (kind, choices) = match &question.kind {
            QuestionKind::Text => ("string", None),
            QuestionKind::Number => ("number", None),
            QuestionKind::Bool => ("boolean", None),
            QuestionKind::Choice { options } => (
                "enum",
                Some(
                    options
                        .iter()
                        .map(|option| serde_json::Value::String(option.value.clone()))
                        .collect::<Vec<_>>(),
                ),
            ),
        };
        let mut entry = serde_json::Map::new();
        entry.insert(
            "id".to_string(),
            serde_json::Value::String(question.id.clone()),
        );
        entry.insert(
            "type".to_string(),
            serde_json::Value::String(kind.to_string()),
        );
        entry.insert(
            "title".to_string(),
            serde_json::Value::String(resolve_component_text(&question.label, catalog, locale)),
        );
        if let Some(help) = question.help.as_ref() {
            entry.insert(
                "description".to_string(),
                serde_json::Value::String(resolve_component_text(help, catalog, locale)),
            );
        }
        entry.insert(
            "required".to_string(),
            serde_json::Value::Bool(question.required),
        );
        if let Some(choice_values) = choices {
            entry.insert(
                "choices".to_string(),
                serde_json::Value::Array(choice_values),
            );
        }
        questions.push(serde_json::Value::Object(entry));
    }

    let form = serde_json::json!({
        "id": "component-setup",
        "title": resolve_component_text(&spec.title, catalog, locale),
        "version": "0.6.0",
        "description": spec.description.as_ref().map(|text| resolve_component_text(text, catalog, locale)),
        "questions": questions,
    });
    serde_json::to_string(&form).context("serialize qa-lib form")
}

fn parse_component_qa_input(question: &serde_json::Value, raw: &str) -> Result<serde_json::Value> {
    let trimmed = raw.trim();
    match question
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("string")
    {
        "boolean" => {
            let lower = trimmed.to_ascii_lowercase();
            Ok(serde_json::Value::Bool(matches!(
                lower.as_str(),
                "true" | "t" | "yes" | "y" | "1"
            )))
        }
        "number" => {
            let parsed = trimmed.parse::<f64>().with_context(|| {
                wizard_t_with("wizard.error.invalid_number", &[("value", trimmed)])
            })?;
            let Some(number) = serde_json::Number::from_f64(parsed) else {
                anyhow::bail!("{}", wizard_t("wizard.error.number_out_of_range"));
            };
            Ok(serde_json::Value::Number(number))
        }
        "integer" => {
            let parsed = trimmed.parse::<i64>().with_context(|| {
                wizard_t_with("wizard.error.invalid_integer", &[("value", trimmed)])
            })?;
            Ok(serde_json::Value::Number(parsed.into()))
        }
        "enum" => {
            let choices = question
                .get("choices")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.enum_choices_missing")))?;
            if let Ok(index) = trimmed.parse::<usize>()
                && index > 0
                && index <= choices.len()
                && let Some(value) = choices[index - 1].as_str()
            {
                return Ok(serde_json::Value::String(value.to_string()));
            }
            if choices
                .iter()
                .any(|choice| choice.as_str().is_some_and(|value| value == trimmed))
            {
                return Ok(serde_json::Value::String(trimmed.to_string()));
            }
            anyhow::bail!(
                "{}",
                wizard_t_with(
                    "wizard.error.invalid_choice",
                    &[(
                        "choices",
                        &choices
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )]
                )
            );
        }
        _ => Ok(serde_json::Value::String(trimmed.to_string())),
    }
}

fn map_from_answers(
    answers: &HashMap<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (key, value) in answers {
        if !value.is_null() {
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

fn collect_pack_flows(pack_dir: &Path) -> Result<Vec<PathBuf>> {
    let flows_root = pack_dir.join("flows");
    if !flows_root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    collect_pack_flows_recursive(&flows_root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_pack_flows_recursive(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if root.is_file() {
        if root.extension() == Some(OsStr::new("ygtc")) {
            out.push(root.to_path_buf());
        }
        return Ok(());
    }
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read directory {}", root.display()))? {
        let path = entry
            .with_context(|| format!("read directory entry in {}", root.display()))?
            .path();
        collect_pack_flows_recursive(&path, out)?;
    }
    Ok(())
}

fn wizard_add_flow_with_io<R: Read, W: Write>(
    pack_dir: &Path,
    reader: &mut R,
    writer: &mut W,
    answers_log: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let questions = vec![
        Question {
            id: "flow.scope".to_string(),
            prompt: wizard_t("wizard.add_flow.scope.prompt"),
            kind: greentic_flow::questions::QuestionKind::Choice,
            required: true,
            default: Some(serde_json::Value::String("global".to_string())),
            choices: vec![
                serde_json::Value::String("global".to_string()),
                serde_json::Value::String("tenant".to_string()),
            ],
            show_if: None,
            writes_to: None,
        },
        Question {
            id: "flow.tenant_id".to_string(),
            prompt: wizard_t("wizard.add_flow.tenant.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: Some(serde_json::json!({"id":"flow.scope","equals":"tenant"})),
            writes_to: None,
        },
        Question {
            id: "flow.team_scope".to_string(),
            prompt: wizard_t("wizard.add_flow.team_scope.prompt"),
            kind: greentic_flow::questions::QuestionKind::Choice,
            required: true,
            default: Some(serde_json::Value::String("all-teams".to_string())),
            choices: vec![
                serde_json::Value::String("all-teams".to_string()),
                serde_json::Value::String("specific-team".to_string()),
            ],
            show_if: Some(serde_json::json!({"id":"flow.scope","equals":"tenant"})),
            writes_to: None,
        },
        Question {
            id: "flow.team_id".to_string(),
            prompt: wizard_t("wizard.add_flow.team.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: Some(serde_json::json!({"id":"flow.team_scope","equals":"specific-team"})),
            writes_to: None,
        },
        Question {
            id: "flow.type".to_string(),
            prompt: wizard_t("wizard.add_flow.type.prompt"),
            kind: greentic_flow::questions::QuestionKind::Choice,
            required: true,
            default: Some(serde_json::Value::String("messaging".to_string())),
            choices: vec![
                serde_json::Value::String("messaging".to_string()),
                serde_json::Value::String("events".to_string()),
            ],
            show_if: None,
            writes_to: None,
        },
        Question {
            id: "flow.name".to_string(),
            prompt: wizard_t("wizard.add_flow.name.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        },
    ];

    let answers =
        run_questions_with_qa_lib_io(&questions, HashMap::new(), &mut *reader, &mut *writer)?;
    for (key, value) in &answers {
        if !value.is_null() {
            answers_log.insert(key.clone(), value.clone());
        }
    }
    let scope = answer_str(&answers, "flow.scope")?;
    let tenant = answers
        .get("flow.tenant_id")
        .and_then(serde_json::Value::as_str);
    let team_scope = answers
        .get("flow.team_scope")
        .and_then(serde_json::Value::as_str);
    let team_id = answers
        .get("flow.team_id")
        .and_then(serde_json::Value::as_str);
    let flow_type = answer_str(&answers, "flow.type")?;
    let flow_name = answer_str(&answers, "flow.name")?;

    let rel_path =
        build_add_flow_relative_path(scope, tenant, team_scope, team_id, flow_type, flow_name)?;
    let abs_path = pack_dir.join(&rel_path);
    let flow_id = flow_id_from_name(flow_name)?;
    write_new_flow_file(NewFlowFileSpec {
        flow_path: abs_path.clone(),
        flow_id,
        flow_type: flow_type.to_string(),
        schema_version: 2,
        name: None,
        description: None,
        force: false,
        backup: false,
    })?;
    writeln!(
        writer,
        "{}",
        wizard_t_with(
            "wizard.add_flow.created",
            &[("path", &abs_path.display().to_string())]
        )
    )
    .ok();
    Ok(())
}

struct NewFlowFileSpec {
    flow_path: PathBuf,
    flow_id: String,
    flow_type: String,
    schema_version: u32,
    name: Option<String>,
    description: Option<String>,
    force: bool,
    backup: bool,
}

fn write_new_flow_file(spec: NewFlowFileSpec) -> Result<()> {
    let doc = greentic_flow::model::FlowDoc {
        id: spec.flow_id,
        title: spec.name,
        description: spec.description,
        flow_type: spec.flow_type,
        start: None,
        parameters: serde_json::Value::Object(Default::default()),
        tags: Vec::new(),
        schema_version: Some(spec.schema_version),
        entrypoints: IndexMap::new(),
        meta: None,
        nodes: IndexMap::new(),
    };
    let mut yaml = serde_yaml_bw::to_string(&doc)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    write_flow_file(&spec.flow_path, &yaml, spec.force, spec.backup)
}

fn wizard_edit_flow_summary_with_io<R: Read, W: Write>(
    flow_path: &Path,
    reader: &mut R,
    writer: &mut W,
    answers_log: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let doc = load_ygtc_from_path(flow_path)?;
    let current_name = resolve_flow_summary_value(flow_path, doc.title.as_deref());
    let current_description = resolve_flow_summary_value(flow_path, doc.description.as_deref());
    let not_set = wizard_t("wizard.flow.summary.not_set");
    writeln!(
        writer,
        "{}",
        wizard_t_with(
            "wizard.flow.summary.current_name",
            &[("value", current_name.as_deref().unwrap_or(not_set.as_str()))]
        )
    )
    .ok();
    writeln!(
        writer,
        "{}",
        wizard_t_with(
            "wizard.flow.summary.current_description",
            &[(
                "value",
                current_description.as_deref().unwrap_or(not_set.as_str())
            )]
        )
    )
    .ok();
    let edit_prompt = format!(
        "{}\n1) {}\n2) {}\nSelect action",
        wizard_t("wizard.flow.summary.edit.prompt"),
        wizard_t("wizard.choice.common.no"),
        wizard_t("wizard.choice.common.yes")
    );
    let edit_answer = wizard_menu_answer(
        &mut *reader,
        &mut *writer,
        "summary.edit",
        &edit_prompt,
        &["1", "2"],
    )?;
    answers_log.insert(
        "summary.edit".to_string(),
        serde_json::Value::String(edit_answer.clone()),
    );
    if edit_answer.trim() != "2" {
        writeln!(writer, "{}", wizard_t("wizard.flow.summary.no_changes")).ok();
        return Ok(());
    }
    let questions = vec![
        Question {
            id: "summary.name".to_string(),
            prompt: wizard_t("wizard.flow.summary.name.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: false,
            default: current_name.map(serde_json::Value::String),
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        },
        Question {
            id: "summary.description".to_string(),
            prompt: wizard_t("wizard.flow.summary.description.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: false,
            default: current_description.map(serde_json::Value::String),
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        },
    ];
    let answers =
        run_questions_with_qa_lib_io(&questions, HashMap::new(), &mut *reader, &mut *writer)?;
    for (key, value) in &answers {
        if !value.is_null() {
            answers_log.insert(key.clone(), value.clone());
        }
    }
    let name = answers
        .get("summary.name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);
    let description = answers
        .get("summary.description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);
    if name.is_none() && description.is_none() {
        writeln!(writer, "{}", wizard_t("wizard.flow.summary.no_changes")).ok();
        return Ok(());
    }
    let flow_id = doc.id.clone();
    let name_key = format!("flow.{flow_id}.title");
    let description_key = format!("flow.{flow_id}.description");
    let mut name_tag = None;
    if let Some(name_value) = name.as_deref() {
        write_pack_translation(flow_path, &name_key, name_value)?;
        name_tag = Some(format!("i18n:{name_key}"));
    }
    let mut description_tag = None;
    if let Some(description_value) = description.as_deref() {
        write_pack_translation(flow_path, &description_key, description_value)?;
        description_tag = Some(format!("i18n:{description_key}"));
    }
    handle_update(
        UpdateArgs {
            flow_path: flow_path.to_path_buf(),
            flow_id: None,
            flow_type: None,
            schema_version: None,
            name: name_tag,
            description: description_tag,
            tags: None,
        },
        false,
    )?;
    writeln!(writer, "{}", wizard_t("wizard.flow.summary.updated")).ok();
    Ok(())
}

fn resolve_flow_summary_value(flow_path: &Path, value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    let Some(key) = raw.strip_prefix("i18n:") else {
        return Some(raw.to_string());
    };
    read_pack_translation(flow_path, key.trim()).or_else(|| Some(raw.to_string()))
}

fn read_pack_translation(flow_path: &Path, key: &str) -> Option<String> {
    let pack_root = infer_pack_root_from_flow_path(flow_path).ok()?;
    let i18n_dir = pack_root.join("i18n");
    for filename in pack_i18n_candidate_files() {
        let path = i18n_dir.join(filename);
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
        else {
            continue;
        };
        if let Some(value) = map.get(key).and_then(serde_json::Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn pack_i18n_candidate_files() -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let locale = resolve_locale(None);
    let normalized = locale.trim().replace('_', "-");
    if !normalized.is_empty() {
        let full = format!("{normalized}.json");
        if seen.insert(full.clone()) {
            files.push(full);
        }
        if let Some((language, _)) = normalized.split_once('-') {
            let lang = format!("{language}.json");
            if seen.insert(lang.clone()) {
                files.push(lang);
            }
        }
    }
    for fallback in ["en.json", "en-GB.json"] {
        let fallback = fallback.to_string();
        if seen.insert(fallback.clone()) {
            files.push(fallback);
        }
    }
    files
}

fn write_pack_translation(flow_path: &Path, key: &str, value: &str) -> Result<()> {
    let pack_root = infer_pack_root_from_flow_path(flow_path)?;
    let i18n_dir = pack_root.join("i18n");
    fs::create_dir_all(&i18n_dir)
        .with_context(|| format!("create directory {}", i18n_dir.display()))?;
    let i18n_path = i18n_dir.join("en-GB.json");
    let mut map = if i18n_path.exists() {
        let text = fs::read_to_string(&i18n_path)
            .with_context(|| format!("read translation file {}", i18n_path.display()))?;
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };
    map.insert(
        key.to_string(),
        serde_json::Value::String(value.to_string()),
    );
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .context("serialize translation file")?;
    fs::write(&i18n_path, text)
        .with_context(|| format!("write translation file {}", i18n_path.display()))?;
    Ok(())
}

fn infer_pack_root_from_flow_path(flow_path: &Path) -> Result<PathBuf> {
    let mut cursor = flow_path.parent();
    while let Some(path) = cursor {
        if path.file_name().and_then(|s| s.to_str()) == Some("flows") {
            let root = path.parent().ok_or_else(|| {
                anyhow!(
                    "{}",
                    wizard_t_with(
                        "wizard.error.flow_path_has_no_pack_root",
                        &[("path", &flow_path.display().to_string())]
                    )
                )
            })?;
            return Ok(root.to_path_buf());
        }
        cursor = path.parent();
    }
    anyhow::bail!(
        "{}",
        wizard_t_with(
            "wizard.error.cannot_infer_pack_root",
            &[("path", &flow_path.display().to_string())]
        )
    )
}

fn wizard_delete_flow_with_io<R: Read, W: Write>(
    flow_path: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let question = Question {
        id: "delete.confirm".to_string(),
        prompt: wizard_t("wizard.flow.delete.confirm.prompt"),
        kind: greentic_flow::questions::QuestionKind::Choice,
        required: true,
        default: Some(serde_json::Value::String("no".to_string())),
        choices: vec![
            serde_json::Value::String("no".to_string()),
            serde_json::Value::String("yes".to_string()),
        ],
        show_if: None,
        writes_to: None,
    };
    let answers =
        run_questions_with_qa_lib_io(&[question], HashMap::new(), &mut *reader, &mut *writer)?;
    let confirm = answers
        .get("delete.confirm")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no");
    if confirm != "yes" {
        writeln!(writer, "{}", wizard_t("wizard.flow.delete.cancelled")).ok();
        return Ok(());
    }

    let sidecar_path = sidecar_path_for_flow(flow_path);
    let wizard_state_path = {
        let base = flow_path.parent().unwrap_or_else(|| Path::new("."));
        base.join(".greentic/cache/flow_wizard")
    };

    fs::remove_file(flow_path).with_context(|| format!("delete flow {}", flow_path.display()))?;
    if sidecar_path.exists() {
        let _ = fs::remove_file(&sidecar_path);
    }
    if wizard_state_path.exists() {
        let _ = fs::remove_dir_all(&wizard_state_path);
    }

    writeln!(
        writer,
        "{}",
        wizard_t_with(
            "wizard.flow.delete.deleted",
            &[("path", &flow_path.display().to_string())]
        )
    )
    .ok();
    Ok(())
}

fn wizard_delete_step_with_io<R: Read, W: Write>(
    flow_path: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let doc = load_ygtc_from_path(flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    if flow_ir.nodes.is_empty() {
        writeln!(writer, "{}", wizard_t("wizard.step.delete.none")).ok();
        return Ok(());
    }
    let mut choices: Vec<serde_json::Value> = flow_ir
        .nodes
        .keys()
        .map(|id| serde_json::Value::String(id.clone()))
        .collect();
    let mut prompt = format!("{}\n", wizard_t("wizard.step.delete.prompt"));
    for (idx, node_id) in flow_ir.nodes.keys().enumerate() {
        prompt.push_str(&format!("{}. {}\n", idx + 1, node_id));
    }
    prompt.push_str(wizard_t("wizard.menu.nav.back").as_str());
    choices.push(serde_json::Value::String("cancel".to_string()));
    let question = Question {
        id: "step.delete.id".to_string(),
        prompt,
        kind: greentic_flow::questions::QuestionKind::Choice,
        required: true,
        default: Some(serde_json::Value::String("cancel".to_string())),
        choices,
        show_if: None,
        writes_to: None,
    };
    let answers =
        run_questions_with_qa_lib_io(&[question], HashMap::new(), &mut *reader, &mut *writer)?;
    let step_id = answers
        .get("step.delete.id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("cancel");
    if step_id == "cancel" {
        writeln!(writer, "{}", wizard_t("wizard.step.delete.cancelled")).ok();
        return Ok(());
    }
    handle_delete_step(
        DeleteStepArgs {
            component_id: None,
            flow_path: flow_path.to_path_buf(),
            step: Some(step_id.to_string()),
            wizard_mode: None,
            answers: None,
            answers_file: None,
            answers_dir: None,
            // Updating a step should replace prior answers artifacts for the same
            // flow/node/mode instead of failing with "answers already exist".
            overwrite_answers: true,
            reask: false,
            locale: None,
            interactive: false,
            component: None,
            local_wasm: None,
            distributor_url: None,
            auth_token: None,
            tenant: None,
            env: None,
            pack: None,
            component_version: None,
            abi_version: None,
            resolver: None,
            strategy: "splice".to_string(),
            multi_pred: "error".to_string(),
            assume_yes: true,
            write: true,
        },
        OutputFormat::Human,
        false,
    )?;
    writeln!(
        writer,
        "{}",
        wizard_t_with("wizard.step.delete.deleted", &[("step", step_id)],)
    )
    .ok();
    Ok(())
}

fn wizard_list_steps_with_io<W: Write>(flow_path: &Path, writer: &mut W) -> Result<()> {
    let doc = load_ygtc_from_path(flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    if flow_ir.nodes.is_empty() {
        writeln!(writer, "{}", wizard_t("wizard.step.list.none")).ok();
        return Ok(());
    }
    writeln!(writer, "{}", wizard_t("wizard.step.list.header")).ok();
    for (idx, node_id) in flow_ir.nodes.keys().enumerate() {
        writeln!(writer, "{}. {}", idx + 1, node_id).ok();
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct WizardStepSourceSelection {
    local_wasm: Option<PathBuf>,
    component_ref: Option<String>,
    pin: bool,
}

fn wizard_add_step_with_io<R: Read, W: Write>(
    pack_dir: &Path,
    flow_path: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let doc = load_ygtc_from_path(flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    let after = wizard_select_add_step_anchor_with_io(&flow_ir, &mut *reader, &mut *writer)?;

    let source = match wizard_select_step_source(pack_dir, reader, writer, true)? {
        Some(source) => source,
        None => {
            writeln!(writer, "{}", wizard_t("wizard.step.add.cancelled")).ok();
            return Ok(());
        }
    };
    let wizard_mode = wizard_select_setup_mode(reader, writer, "add")?;
    let Some(wizard_mode) = wizard_mode else {
        writeln!(writer, "{}", wizard_t("wizard.step.add.cancelled")).ok();
        return Ok(());
    };

    let resolver = env::var("GREENTIC_FLOW_WIZARD_RESOLVER")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mut qa_io = QaInteractiveIo {
        reader: &mut *reader,
        writer: &mut *writer,
    };
    handle_add_step_with_qa_io(
        AddStepArgs {
            component_id: None,
            flow_path: flow_path.to_path_buf(),
            after,
            mode: AddStepMode::Default,
            pack_alias: None,
            wizard_mode: Some(wizard_mode),
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
            interactive: true,
            allow_cycles: false,
            dry_run: false,
            write: false,
            validate_only: false,
            manifests: Vec::new(),
            node_id: None,
            component_ref: source.component_ref,
            local_wasm: source.local_wasm,
            distributor_url: None,
            auth_token: None,
            tenant: None,
            env: None,
            pack: None,
            component_version: None,
            abi_version: None,
            resolver,
            pin: source.pin,
            allow_contract_change: false,
        },
        SchemaMode::Strict,
        OutputFormat::Human,
        false,
        Some(&mut qa_io),
    )?;
    writeln!(writer, "{}", wizard_t("wizard.step.add.done")).ok();
    Ok(())
}

fn wizard_select_add_step_anchor_with_io<R: Read, W: Write>(
    flow_ir: &FlowIr,
    reader: &mut R,
    writer: &mut W,
) -> Result<Option<String>> {
    let anchors: Vec<String> = flow_ir.nodes.keys().cloned().collect();
    if anchors.is_empty() {
        return Ok(None);
    }

    let mut prompt = format!("{}\n", wizard_t("wizard.step.add.after.prompt"));
    let mut options = vec![wizard_t("wizard.choice.step.after.auto")];
    options.extend(anchors.iter().cloned());
    for (idx, option) in options.iter().enumerate() {
        prompt.push_str(&format!("{}. {}\n", idx + 1, option));
    }
    prompt.push_str("Select anchor");

    let choice_values: Vec<String> = (1..=options.len()).map(|n| n.to_string()).collect();
    let choice_refs: Vec<&str> = choice_values.iter().map(String::as_str).collect();
    let answer = wizard_menu_answer(
        &mut *reader,
        &mut *writer,
        "step.add.after",
        &prompt,
        &choice_refs,
    )?;
    let selected_idx = answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|idx| *idx >= 1 && *idx <= options.len())
        .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.invalid_number")))?;
    if selected_idx == 1 {
        return Ok(None);
    }
    Ok(anchors.get(selected_idx - 2).cloned())
}

fn wizard_update_step_with_io<R: Read, W: Write>(
    pack_dir: &Path,
    flow_path: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    let doc = load_ygtc_from_path(flow_path)?;
    let flow_ir = FlowIr::from_doc(doc)?;
    if flow_ir.nodes.is_empty() {
        writeln!(writer, "{}", wizard_t("wizard.step.update.none")).ok();
        return Ok(());
    }

    let mut step_choices: Vec<serde_json::Value> = flow_ir
        .nodes
        .keys()
        .map(|id| serde_json::Value::String(id.clone()))
        .collect();
    let mut prompt = format!("{}\n", wizard_t("wizard.step.update.select.prompt"));
    for (idx, node_id) in flow_ir.nodes.keys().enumerate() {
        prompt.push_str(&format!("{}. {}\n", idx + 1, node_id));
    }
    prompt.push_str(wizard_t("wizard.menu.nav.back").as_str());
    step_choices.push(serde_json::Value::String("cancel".to_string()));
    let step_answers = run_questions_with_qa_lib_io(
        &[Question {
            id: "step.update.id".to_string(),
            prompt,
            kind: greentic_flow::questions::QuestionKind::Choice,
            required: true,
            default: Some(serde_json::Value::String("cancel".to_string())),
            choices: step_choices,
            show_if: None,
            writes_to: None,
        }],
        HashMap::new(),
        &mut *reader,
        &mut *writer,
    )?;
    let step_id = step_answers
        .get("step.update.id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("cancel")
        .to_string();
    if step_id == "cancel" {
        writeln!(writer, "{}", wizard_t("wizard.step.update.cancelled")).ok();
        return Ok(());
    }

    let source = match wizard_select_step_source(pack_dir, reader, writer, false)? {
        Some(source) => source,
        None => {
            writeln!(writer, "{}", wizard_t("wizard.step.update.cancelled")).ok();
            return Ok(());
        }
    };
    let wizard_mode = wizard_select_setup_mode(reader, writer, "update")?;
    let Some(wizard_mode) = wizard_mode else {
        writeln!(writer, "{}", wizard_t("wizard.step.update.cancelled")).ok();
        return Ok(());
    };

    let resolver = env::var("GREENTIC_FLOW_WIZARD_RESOLVER")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mut qa_io = QaInteractiveIo {
        reader: &mut *reader,
        writer: &mut *writer,
    };
    handle_update_step_with_qa_io(
        UpdateStepArgs {
            component_id: None,
            flow_path: flow_path.to_path_buf(),
            step: Some(step_id.clone()),
            mode: "default".to_string(),
            wizard_mode: Some(wizard_mode),
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
            non_interactive: false,
            interactive: true,
            component: source.component_ref,
            local_wasm: source.local_wasm,
            distributor_url: None,
            auth_token: None,
            tenant: None,
            env: None,
            pack: None,
            component_version: None,
            abi_version: None,
            resolver,
            dry_run: false,
            write: false,
            allow_contract_change: false,
        },
        SchemaMode::Strict,
        OutputFormat::Human,
        false,
        Some(&mut qa_io),
    )?;
    writeln!(
        writer,
        "{}",
        wizard_t_with("wizard.step.update.done", &[("step", &step_id)],)
    )
    .ok();
    Ok(())
}

fn wizard_select_setup_mode<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prefix: &str,
) -> Result<Option<WizardModeArg>> {
    let answers = run_questions_with_qa_lib_io(
        &[Question {
            id: format!("step.{prefix}.setup_mode"),
            prompt: wizard_t("wizard.step.setup_mode.prompt"),
            kind: greentic_flow::questions::QuestionKind::Choice,
            required: true,
            default: Some(serde_json::Value::String("default".to_string())),
            choices: vec![
                serde_json::Value::String("default".to_string()),
                serde_json::Value::String("personalised".to_string()),
                serde_json::Value::String("cancel".to_string()),
            ],
            show_if: None,
            writes_to: None,
        }],
        HashMap::new(),
        &mut *reader,
        &mut *writer,
    )?;
    let mode = answers
        .get(&format!("step.{prefix}.setup_mode"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("cancel");
    let mapped = match mode {
        "default" => Some(WizardModeArg::Default),
        "personalised" => Some(WizardModeArg::Setup),
        _ => None,
    };
    Ok(mapped)
}

fn wizard_generate_translations_with_io<R: Read, W: Write>(
    pack_dir: &Path,
    reader: &mut R,
    writer: &mut W,
    answers_log: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let answers = run_questions_with_qa_lib_io(
        &[Question {
            id: "wizard.translate.locales".to_string(),
            prompt: wizard_t("wizard.translate.locales.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        }],
        HashMap::new(),
        &mut *reader,
        &mut *writer,
    )?;
    let locales = answer_str(&answers, "wizard.translate.locales")?.to_string();
    answers_log.insert(
        "wizard.translate.locales".to_string(),
        serde_json::Value::String(locales.clone()),
    );
    let locale_list = parse_locale_list(&locales);
    if locale_list.is_empty() {
        anyhow::bail!("{}", wizard_t("wizard.translate.invalid_locales"));
    }

    let en_path = pack_dir.join("i18n/en-GB.json");
    if !en_path.exists() {
        anyhow::bail!("{}", wizard_t("wizard.translate.missing_source"));
    }
    if env::var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB")
        .ok()
        .as_deref()
        == Some("1")
    {
        generate_translations_stub(pack_dir, &locale_list)?;
        writeln!(writer, "{}", wizard_t("wizard.translate.done")).ok();
        return Ok(());
    }
    let i18n = greentic_i18n_translator::cli_i18n::CliI18n::from_request(Some("en"))
        .map_err(anyhow::Error::msg)?;
    let cli = greentic_i18n_translator::cli::Cli {
        locale: Some("en".to_string()),
        command: greentic_i18n_translator::cli::Command::Translate {
            langs: locale_list.join(","),
            en: en_path,
            auth_mode: greentic_i18n_translator::cli::CliAuthMode::Auto,
            codex_home: None,
            batch_size: 50,
            max_retries: 2,
            glossary: None,
            api_key_stdin: false,
            overwrite_manual: false,
            cache_dir: None,
        },
    };
    greentic_i18n_translator::cli::run_with(cli, &i18n).map_err(anyhow::Error::msg)?;
    writeln!(writer, "{}", wizard_t("wizard.translate.done")).ok();
    Ok(())
}

fn parse_locale_list(raw: &str) -> Vec<String> {
    let mut out = Vec::<String>::new();
    for locale in raw.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if !out.iter().any(|existing| existing == locale) {
            out.push(locale.to_string());
        }
    }
    out
}

fn generate_translations_stub(pack_dir: &Path, locales: &[String]) -> Result<()> {
    let i18n_dir = pack_dir.join("i18n");
    fs::create_dir_all(&i18n_dir)
        .with_context(|| format!("create directory {}", i18n_dir.display()))?;
    let en_path = i18n_dir.join("en-GB.json");
    let en_map = fs::read_to_string(&en_path)
        .with_context(|| format!("read translation source {}", en_path.display()))
        .and_then(|text| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
                .context("parse source translation json")
        })?;
    for locale in locales {
        if locale == "en" || locale == "en-GB" {
            continue;
        }
        let mut target = serde_json::Map::new();
        for (key, value) in &en_map {
            let base = value.as_str().unwrap_or_default();
            target.insert(
                key.clone(),
                serde_json::Value::String(format!("{base} [{locale}]")),
            );
        }
        let out_path = i18n_dir.join(format!("{locale}.json"));
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(target))
            .context("serialize stub translation json")?;
        fs::write(&out_path, text)
            .with_context(|| format!("write translation file {}", out_path.display()))?;
    }
    Ok(())
}

fn wizard_select_step_source<R: Read, W: Write>(
    pack_dir: &Path,
    reader: &mut R,
    writer: &mut W,
    ask_pin_for_remote: bool,
) -> Result<Option<WizardStepSourceSelection>> {
    let source_answers = run_questions_with_qa_lib_io(
        &[Question {
            id: "step.source.kind".to_string(),
            prompt: wizard_t("wizard.step.source.kind.prompt"),
            kind: greentic_flow::questions::QuestionKind::Choice,
            required: true,
            default: Some(serde_json::Value::String("local".to_string())),
            choices: vec![
                serde_json::Value::String("local".to_string()),
                serde_json::Value::String("remote".to_string()),
                serde_json::Value::String("cancel".to_string()),
            ],
            show_if: None,
            writes_to: None,
        }],
        HashMap::new(),
        &mut *reader,
        &mut *writer,
    )?;
    let source_kind = source_answers
        .get("step.source.kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("cancel");
    if source_kind == "cancel" {
        return Ok(None);
    }

    if source_kind == "local" {
        let candidates = collect_pack_component_wasms(pack_dir)?;
        let selected_path = if candidates.is_empty() {
            let local_answers = run_questions_with_qa_lib_io(
                &[Question {
                    id: "step.source.local_path".to_string(),
                    prompt: wizard_t("wizard.step.source.local.prompt"),
                    kind: greentic_flow::questions::QuestionKind::String,
                    required: true,
                    default: None,
                    choices: Vec::new(),
                    show_if: None,
                    writes_to: None,
                }],
                HashMap::new(),
                &mut *reader,
                &mut *writer,
            )?;
            PathBuf::from(answer_str(&local_answers, "step.source.local_path")?)
        } else {
            let mut choices = Vec::new();
            for candidate in &candidates {
                let display = candidate
                    .strip_prefix(pack_dir)
                    .unwrap_or(candidate)
                    .display()
                    .to_string();
                choices.push(serde_json::Value::String(display));
            }
            choices.push(serde_json::Value::String("custom".to_string()));
            choices.push(serde_json::Value::String("cancel".to_string()));
            let choice_answers = run_questions_with_qa_lib_io(
                &[Question {
                    id: "step.source.local_choice".to_string(),
                    prompt: wizard_t("wizard.step.source.local.choice.prompt"),
                    kind: greentic_flow::questions::QuestionKind::Choice,
                    required: true,
                    default: choices.first().cloned(),
                    choices: choices.clone(),
                    show_if: None,
                    writes_to: None,
                }],
                HashMap::new(),
                &mut *reader,
                &mut *writer,
            )?;
            let selected = choice_answers
                .get("step.source.local_choice")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("cancel");
            if selected == "cancel" {
                return Ok(None);
            }
            if selected == "custom" {
                let local_answers = run_questions_with_qa_lib_io(
                    &[Question {
                        id: "step.source.local_custom_path".to_string(),
                        prompt: wizard_t("wizard.step.source.local.prompt"),
                        kind: greentic_flow::questions::QuestionKind::String,
                        required: true,
                        default: None,
                        choices: Vec::new(),
                        show_if: None,
                        writes_to: None,
                    }],
                    HashMap::new(),
                    &mut *reader,
                    &mut *writer,
                )?;
                PathBuf::from(answer_str(&local_answers, "step.source.local_custom_path")?)
            } else {
                let rel = PathBuf::from(selected);
                if rel.is_absolute() {
                    rel
                } else {
                    pack_dir.join(rel)
                }
            }
        };
        let copied_local = copy_local_wasm_into_pack_components(pack_dir, &selected_path)?;
        return Ok(Some(WizardStepSourceSelection {
            local_wasm: Some(copied_local),
            component_ref: None,
            pin: true,
        }));
    }

    let ref_answers = run_questions_with_qa_lib_io(
        &[Question {
            id: "step.source.remote_ref".to_string(),
            prompt: wizard_t("wizard.step.source.remote.prompt"),
            kind: greentic_flow::questions::QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        }],
        HashMap::new(),
        &mut *reader,
        &mut *writer,
    )?;
    let remote_ref = answer_str(&ref_answers, "step.source.remote_ref")?.to_string();
    let pin = if ask_pin_for_remote {
        let pin_answers = run_questions_with_qa_lib_io(
            &[Question {
                id: "step.source.remote_pin".to_string(),
                prompt: wizard_t("wizard.step.source.remote.pin.prompt"),
                kind: greentic_flow::questions::QuestionKind::Choice,
                required: true,
                default: Some(serde_json::Value::String("yes".to_string())),
                choices: vec![
                    serde_json::Value::String("yes".to_string()),
                    serde_json::Value::String("no".to_string()),
                ],
                show_if: None,
                writes_to: None,
            }],
            HashMap::new(),
            &mut *reader,
            &mut *writer,
        )?;
        pin_answers
            .get("step.source.remote_pin")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("yes")
            == "yes"
    } else {
        false
    };
    Ok(Some(WizardStepSourceSelection {
        local_wasm: None,
        component_ref: Some(remote_ref),
        pin,
    }))
}

fn copy_local_wasm_into_pack_components(pack_dir: &Path, selected_path: &Path) -> Result<PathBuf> {
    let src_abs = if selected_path.is_absolute() {
        selected_path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(selected_path)
    };
    let src_abs = fs::canonicalize(&src_abs)
        .with_context(|| format!("resolve local wasm path {}", src_abs.display()))?;
    if !src_abs.exists() {
        anyhow::bail!(
            "{}",
            wizard_t_with(
                "wizard.error.local_wasm_missing",
                &[("path", &src_abs.display().to_string())]
            )
        );
    }
    let components_dir = pack_dir.join("components");
    fs::create_dir_all(&components_dir)
        .with_context(|| format!("create directory {}", components_dir.display()))?;
    let digest = compute_local_digest(&src_abs)?;
    let stem = src_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("component");
    let dest = components_dir.join(format!("{stem}-{}.wasm", &digest[..12]));
    if !dest.exists() {
        fs::copy(&src_abs, &dest)
            .with_context(|| format!("copy wasm {} -> {}", src_abs.display(), dest.display()))?;
    }
    Ok(dest)
}

fn collect_pack_component_wasms(pack_dir: &Path) -> Result<Vec<PathBuf>> {
    let components_dir = pack_dir.join("components");
    if !components_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    collect_component_wasms_recursive(&components_dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_component_wasms_recursive(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if root.is_file() {
        if root.extension() == Some(OsStr::new("wasm")) {
            out.push(root.to_path_buf());
        }
        return Ok(());
    }
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read directory {}", root.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", root.display()))?;
        collect_component_wasms_recursive(&entry.path(), out)?;
    }
    Ok(())
}

fn answer_str<'a>(answers: &'a HashMap<String, serde_json::Value>, key: &str) -> Result<&'a str> {
    answers
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                wizard_t_with("wizard.error.missing_required_answer", &[("key", key)])
            )
        })
}

fn flow_id_from_name(name: &str) -> Result<String> {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or(name)
        .trim();
    if stem.is_empty() {
        anyhow::bail!("{}", wizard_t("wizard.error.flow_name_empty"));
    }
    Ok(stem.replace(' ', "-"))
}

fn ensure_ygtc_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{}", wizard_t("wizard.error.flow_name_empty"));
    }
    if trimmed.ends_with(".ygtc") {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{trimmed}.ygtc"))
    }
}

fn build_add_flow_relative_path(
    scope: &str,
    tenant_id: Option<&str>,
    team_scope: Option<&str>,
    team_id: Option<&str>,
    flow_type: &str,
    flow_name: &str,
) -> Result<PathBuf> {
    if flow_type != "messaging" && flow_type != "events" {
        anyhow::bail!(
            "{}",
            wizard_t_with(
                "wizard.error.flow_type_unsupported",
                &[("flow_type", flow_type)]
            )
        );
    }
    let file_name = ensure_ygtc_name(flow_name)?;
    let path = match scope {
        "global" => PathBuf::from("flows")
            .join("global")
            .join(flow_type)
            .join(file_name),
        "tenant" => {
            let tenant = tenant_id
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.tenant_id_required")))?;
            let team_segment = match team_scope {
                Some("all-teams") | None => "all-teams".to_string(),
                Some("specific-team") => {
                    let team = team_id
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| anyhow!("{}", wizard_t("wizard.error.team_id_required")))?;
                    team.to_string()
                }
                Some(other) => {
                    anyhow::bail!(
                        "{}",
                        wizard_t_with("wizard.error.team_scope_unsupported", &[("scope", other)])
                    )
                }
            };
            PathBuf::from("flows")
                .join(tenant)
                .join(team_segment)
                .join(flow_type)
                .join(file_name)
        }
        other => {
            anyhow::bail!(
                "{}",
                wizard_t_with("wizard.error.flow_scope_unsupported", &[("scope", other)])
            )
        }
    };
    Ok(path)
}

fn wizard_catalog_for_locale(locale: &str) -> I18nCatalog {
    static CATALOGS: OnceLock<Mutex<HashMap<String, I18nCatalog>>> = OnceLock::new();
    let catalogs = CATALOGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = catalogs.lock().expect("wizard catalogs lock");
    if let Some(cached) = guard.get(locale) {
        return cached.clone();
    }
    let mut catalog = I18nCatalog::default();
    merge_wizard_i18n_json_embedded(&mut catalog, "en");
    if locale != "en" {
        merge_wizard_i18n_json_embedded(&mut catalog, locale);
    }
    if let Some((language, _)) = locale.split_once('-')
        && !language.is_empty()
        && language != locale
        && language != "en"
    {
        merge_wizard_i18n_json_embedded(&mut catalog, language);
    }
    guard.insert(locale.to_string(), catalog.clone());
    catalog
}

fn wizard_resolved_map_for_locale(locale: &str) -> std::collections::BTreeMap<String, String> {
    static RESOLVED: OnceLock<Mutex<HashMap<String, std::collections::BTreeMap<String, String>>>> =
        OnceLock::new();
    let resolved_cache = RESOLVED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = resolved_cache.lock().expect("wizard resolved map lock");
    if let Some(cached) = guard.get(locale) {
        return cached.clone();
    }
    let catalog = wizard_catalog_for_locale(locale);
    let mut resolved = std::collections::BTreeMap::new();
    for key in embedded_wizard_keys() {
        let value = resolve_cli_text(&catalog, locale, &key, &key);
        if value != key {
            resolved.insert(key.clone(), value.clone());
            resolved.insert(format!("{locale}:{key}"), value.clone());
            resolved.insert(format!("{locale}/{key}"), value);
        }
    }
    guard.insert(locale.to_string(), resolved.clone());
    resolved
}

fn wizard_qa_i18n_config_for_locale(locale: &str) -> QaI18nConfig {
    QaI18nConfig {
        locale: Some(locale.to_string()),
        resolved: Some(wizard_resolved_map_for_locale(locale)),
        debug: false,
    }
}

fn merge_wizard_i18n_json_embedded(catalog: &mut I18nCatalog, locale: &str) {
    let file_name = format!("{locale}.json");
    let Some(file) = EMBEDDED_WIZARD_I18N_DIR.get_file(&file_name) else {
        return;
    };
    let Some(text) = file.contents_utf8() else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(entries) = value.as_object() else {
        return;
    };
    for (key, value) in entries {
        if let Some(message) = value.as_str() {
            catalog.insert(key.clone(), locale.to_string(), message.to_string());
        }
    }
}

fn embedded_wizard_keys() -> Vec<String> {
    let file = EMBEDDED_WIZARD_I18N_DIR
        .get_file("en.json")
        .or_else(|| EMBEDDED_WIZARD_I18N_DIR.get_file("en-GB.json"));
    let Some(file) = file else {
        return Vec::new();
    };
    let Some(text) = file.contents_utf8() else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(entries) = value.as_object() else {
        return Vec::new();
    };
    entries.keys().cloned().collect()
}

fn wizard_t(key: &str) -> String {
    let locale = resolve_locale(None);
    let catalog = wizard_catalog_for_locale(&locale);
    let value = resolve_cli_text(&catalog, &locale, key, key);
    if value == key {
        format!("[[missing:{key}]]")
    } else {
        value
    }
}

fn wizard_t_with(key: &str, replacements: &[(&str, &str)]) -> String {
    let mut value = wizard_t(key);
    for (name, replacement) in replacements {
        value = value.replace(&format!("{{{name}}}"), replacement);
    }
    value
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
    write_new_flow_file(NewFlowFileSpec {
        flow_path: args.flow_path.clone(),
        flow_id: args.flow_id.clone(),
        flow_type: args.flow_type.clone(),
        schema_version: args.schema_version,
        name: args.name,
        description: args.description,
        force: args.force,
        backup,
    })?;
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
        let (config_schema, computed_meta) = if !spec.describe_cbor.is_empty() {
            let (describe, meta) = derive_contract_meta(&spec.describe_cbor, &operation_id)?;
            (Some(describe.config_schema), meta)
        } else if let Some(descriptor) = spec.descriptor.as_ref() {
            derive_contract_meta_from_descriptor(descriptor, &operation_id)?
        } else {
            diags.push(ContractDiagnostic {
                code: "FLOW_CONTRACT_SKIPPED",
                severity: ContractSeverity::Warning,
                message:
                    "descriptor and describe_cbor are both unavailable; skipping contract checks"
                        .to_string(),
                node_id: node_id.clone(),
            });
            continue;
        };

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
        let Some(schema) = config_schema else {
            diags.push(ContractDiagnostic {
                code: "FLOW_SCHEMA_MISSING",
                severity: ContractSeverity::Warning,
                message: "missing inline input schema in descriptor for online validation"
                    .to_string(),
                node_id: node_id.clone(),
            });
            continue;
        };
        let schema_diags = validate_value_against_schema(
            &schema,
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
                let i18n_tag_errors = lint_i18n_tag_fields(path);
                if !i18n_tag_errors.is_empty() {
                    *failures += 1;
                    had_errors = true;
                    for err in i18n_tag_errors {
                        eprintln!("ERR  {}: {err}", path.display());
                    }
                }
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

fn lint_i18n_tag_fields(path: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    let Ok(doc) = load_ygtc_from_path(path) else {
        return errors;
    };
    let i18n_source = load_pack_i18n_source_for_flow(path);
    if let Some(title) = doc.title.as_deref() {
        lint_i18n_tag_value("title", title, i18n_source.as_ref(), &mut errors);
    }
    if let Some(description) = doc.description.as_deref() {
        lint_i18n_tag_value(
            "description",
            description,
            i18n_source.as_ref(),
            &mut errors,
        );
    }
    errors
}

fn lint_i18n_tag_value(
    field: &str,
    value: &str,
    i18n_source: Option<&serde_json::Map<String, serde_json::Value>>,
    errors: &mut Vec<String>,
) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(key) = trimmed.strip_prefix("i18n:") else {
        errors.push(format!(
            "{field} must be an i18n tag (expected prefix i18n:)"
        ));
        return;
    };
    let key = key.trim();
    if key.is_empty() {
        errors.push(format!("{field} i18n tag key cannot be empty"));
        return;
    }
    if let Some(source) = i18n_source {
        match source.get(key).and_then(serde_json::Value::as_str) {
            Some(v) if !v.trim().is_empty() => {}
            _ => errors.push(format!(
                "{field} i18n key '{key}' missing from pack i18n/en-GB.json"
            )),
        }
    }
}

fn load_pack_i18n_source_for_flow(
    path: &Path,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let pack_root = infer_pack_root_from_flow_path(path).ok()?;
    let i18n_path = pack_root.join("i18n/en-GB.json");
    let text = fs::read_to_string(i18n_path).ok()?;
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok()
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
        let config_payload = extract_config_value(&payload);
        for err in validator.iter_errors(&config_payload) {
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
                    let i18n_errors = lint_i18n_tag_fields(path);
                    errors.extend(i18n_errors);
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
    use super::UpdateArgs;
    use super::UpdateStepArgs;
    use super::WizardModeArg;
    use super::handle_add_step;
    use super::handle_delete_step;
    use super::handle_new;
    use super::handle_update;
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
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use tempfile::NamedTempFile;
    use tempfile::tempdir;

    fn extract_config_payload(payload: &serde_json::Value) -> &serde_json::Value {
        payload
            .get("config")
            .and_then(|value| value.as_object().map(|_| value))
            .unwrap_or(payload)
    }

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock")
    }

    #[test]
    fn wizard_menu_main_zero_exits() {
        let dir = tempdir().expect("temp dir");
        let input = Cursor::new("0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard exit");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Main Menu"));
    }

    #[test]
    fn wizard_menu_m_returns_to_main_menu() {
        let dir = tempdir().expect("temp dir");
        let flows_dir = dir.path().join("flows");
        fs::create_dir_all(&flows_dir).expect("create flows dir");
        fs::write(
            flows_dir.join("sample.ygtc"),
            "id: sample\ntype: messaging\nnodes: {}\n",
        )
        .expect("write flow");

        // 2 => edit/delete flows, M => main menu, 0 => exit
        let input = Cursor::new("2\nM\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard navigation");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Select number of flow"));
        assert!(rendered.matches("Main Menu").count() >= 2);
    }

    #[test]
    fn wizard_menu_flow_ops_zero_returns_to_flow_select() {
        let dir = tempdir().expect("temp dir");
        let flows_dir = dir.path().join("flows");
        fs::create_dir_all(&flows_dir).expect("create flows dir");
        fs::write(
            flows_dir.join("sample.ygtc"),
            "id: sample\ntype: messaging\nnodes: {}\n",
        )
        .expect("write flow");

        // 2 => flow list, 1 => flow ops, 0 => back to flow list, 0 => main, 0 => exit.
        let input = Cursor::new("2\n1\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard flow-ops back");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Select number of flow"));
        assert!(rendered.contains("Select operation"));
    }

    #[test]
    fn wizard_menu_flow_ops_m_returns_to_main_menu() {
        let dir = tempdir().expect("temp dir");
        let flows_dir = dir.path().join("flows");
        fs::create_dir_all(&flows_dir).expect("create flows dir");
        fs::write(
            flows_dir.join("sample.ygtc"),
            "id: sample\ntype: messaging\nnodes: {}\n",
        )
        .expect("write flow");

        // 2 => flow list, 1 => flow ops, M => main, 0 => exit.
        let input = Cursor::new("2\n1\nM\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard flow-ops main menu");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.matches("Main Menu").count() >= 2);
    }

    #[test]
    fn wizard_generate_translations_requires_source_bundle() {
        let dir = tempdir().expect("temp dir");
        let input = Cursor::new("3\nes\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard should continue after translation error");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("missing i18n/en-GB.json"));
    }

    #[test]
    fn wizard_generate_translations_stub_writes_locale_files() {
        let _guard = env_test_lock();
        let dir = tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("i18n")).expect("create i18n");
        fs::write(
            dir.path().join("i18n/en-GB.json"),
            r#"{"wizard.hello":"Hello"}"#,
        )
        .expect("write source bundle");

        let previous = env::var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB").ok();
        unsafe {
            env::set_var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB", "1");
        }
        let mut input = Cursor::new("es,fr\n");
        let mut output = Vec::new();
        let mut answers_log = serde_json::Map::new();
        super::wizard_generate_translations_with_io(
            dir.path(),
            &mut input,
            &mut output,
            &mut answers_log,
        )
        .expect("generate translations (stub)");
        if let Some(value) = previous {
            unsafe {
                env::set_var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB", value);
            }
        } else {
            unsafe {
                env::remove_var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB");
            }
        }

        let es_text = fs::read_to_string(dir.path().join("i18n/es.json")).expect("read es");
        let fr_text = fs::read_to_string(dir.path().join("i18n/fr.json")).expect("read fr");
        assert!(es_text.contains("Hello [es]"));
        assert!(fr_text.contains("Hello [fr]"));
    }

    #[test]
    fn wizard_menu_generate_translations_pack_wide_and_save() {
        let _guard = env_test_lock();
        let dir = tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("i18n")).expect("create i18n");
        fs::write(
            dir.path().join("i18n/en-GB.json"),
            r#"{"flow.alpha.title":"Alpha","flow.beta.title":"Beta"}"#,
        )
        .expect("write source bundle");

        let previous = env::var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB").ok();
        unsafe {
            env::set_var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB", "1");
        }

        // 3=Generate translations, enter locales, 4=Save, enter default answers path, 0=Exit.
        let input = Cursor::new("3\nes,fr\n4\n\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard menu generate translations");

        if let Some(value) = previous {
            unsafe {
                env::set_var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB", value);
            }
        } else {
            unsafe {
                env::remove_var("GREENTIC_FLOW_WIZARD_TRANSLATE_STUB");
            }
        }

        let es_text = fs::read_to_string(dir.path().join("i18n/es.json")).expect("read es");
        let fr_text = fs::read_to_string(dir.path().join("i18n/fr.json")).expect("read fr");
        let es_json: serde_json::Value = serde_json::from_str(&es_text).expect("parse es");
        let fr_json: serde_json::Value = serde_json::from_str(&fr_text).expect("parse fr");
        assert!(
            es_json
                .get("flow.alpha.title")
                .and_then(|v| v.as_str())
                .is_some()
        );
        assert!(
            es_json
                .get("flow.beta.title")
                .and_then(|v| v.as_str())
                .is_some()
        );
        assert!(
            fr_json
                .get("flow.alpha.title")
                .and_then(|v| v.as_str())
                .is_some()
        );
        assert!(
            fr_json
                .get("flow.beta.title")
                .and_then(|v| v.as_str())
                .is_some()
        );
    }

    #[test]
    fn wizard_i18n_smoke_no_missing_key_markers() {
        let keys = [
            "wizard.menu.main.prompt",
            "wizard.menu.flow_select.title",
            "wizard.menu.flow_ops.prompt",
            "wizard.menu.nav.back",
            "wizard.menu.nav.main",
            "wizard.add_flow.scope.prompt",
            "wizard.add_flow.created",
            "wizard.flow.summary.name.prompt",
            "wizard.flow.summary.edit.prompt",
            "wizard.flow.summary.current_name",
            "wizard.flow.summary.current_description",
            "wizard.flow.summary.not_set",
            "wizard.flow.summary.updated",
            "wizard.flow.delete.confirm.prompt",
            "wizard.flow.delete.deleted",
            "wizard.step.add.after.prompt",
            "wizard.step.add.done",
            "wizard.step.update.select.prompt",
            "wizard.step.update.done",
            "wizard.step.setup_mode.prompt",
            "wizard.step.source.kind.prompt",
            "wizard.step.source.local.prompt",
            "wizard.step.source.remote.prompt",
            "wizard.save.done",
            "wizard.save.dry_run_done",
            "wizard.save.empty_flow",
            "wizard.answers.path.prompt",
            "wizard.answers.path.saved",
            "wizard.translate.locales.prompt",
            "wizard.translate.done",
            "wizard.translate.missing_source",
            "wizard.translate.invalid_locales",
            "wizard.step.delete.prompt",
            "wizard.step.delete.deleted",
            "wizard.step.list.header",
            "wizard.step.list.none",
            "wizard.error.pack_dir_not_found",
            "wizard.error.missing_answer_for_question",
            "wizard.error.flow_path_has_no_pack_root",
            "wizard.error.cannot_infer_pack_root",
            "wizard.error.local_wasm_missing",
            "wizard.error.missing_required_answer",
            "wizard.error.flow_name_empty",
            "wizard.error.flow_type_unsupported",
            "wizard.error.tenant_id_required",
            "wizard.error.team_id_required",
            "wizard.error.team_scope_unsupported",
            "wizard.error.flow_scope_unsupported",
            "wizard.error.invalid_choice",
            "wizard.error.required_input",
            "wizard.error.invalid_integer",
            "wizard.error.invalid_number",
            "wizard.error.number_out_of_range",
            "wizard.error.enum_choices_missing",
            "wizard.error.invalid_utf8_input",
            "wizard.error.qa_runner_failed",
            "wizard.qa.prompt.select_option",
            "wizard.qa.prompt.enter_true_false",
            "wizard.qa.prompt.enter_number",
            "wizard.qa.prompt.enter_integer",
            "wizard.qa.prompt.enter_text",
            "wizard.choice.flow.scope.global",
            "wizard.choice.flow.scope.tenant",
            "wizard.choice.flow.team_scope.all_teams",
            "wizard.choice.flow.team_scope.specific_team",
            "wizard.choice.flow.type.messaging",
            "wizard.choice.flow.type.events",
            "wizard.choice.common.yes",
            "wizard.choice.common.no",
            "wizard.choice.common.cancel",
            "wizard.choice.setup.default",
            "wizard.choice.setup.personalised",
            "wizard.choice.source.local",
            "wizard.choice.source.remote",
            "wizard.choice.source.custom",
            "wizard.choice.step.after.auto",
        ];
        for key in keys {
            let value = super::wizard_t(key);
            assert!(
                !value.contains("[[missing:"),
                "expected key to resolve without missing marker: {key}"
            );
        }
    }

    #[test]
    fn add_flow_path_global_messaging() {
        let path =
            super::build_add_flow_relative_path("global", None, None, None, "messaging", "welcome")
                .expect("global path");
        assert_eq!(path, PathBuf::from("flows/global/messaging/welcome.ygtc"));
    }

    #[test]
    fn add_flow_path_tenant_all_teams_events() {
        let path = super::build_add_flow_relative_path(
            "tenant",
            Some("tenant-a"),
            Some("all-teams"),
            None,
            "events",
            "audit",
        )
        .expect("tenant all-teams path");
        assert_eq!(
            path,
            PathBuf::from("flows/tenant-a/all-teams/events/audit.ygtc")
        );
    }

    #[test]
    fn add_flow_path_tenant_specific_team() {
        let path = super::build_add_flow_relative_path(
            "tenant",
            Some("tenant-a"),
            Some("specific-team"),
            Some("blue"),
            "messaging",
            "alerts",
        )
        .expect("tenant team path");
        assert_eq!(
            path,
            PathBuf::from("flows/tenant-a/blue/messaging/alerts.ygtc")
        );
    }

    #[test]
    fn add_flow_path_rejects_unknown_type() {
        let err = super::build_add_flow_relative_path("global", None, None, None, "http", "x")
            .expect_err("invalid type");
        assert!(err.to_string().to_lowercase().contains("flow type"));
    }

    #[test]
    fn wizard_add_flow_menu_creates_global_flow() {
        let dir = tempdir().expect("temp dir");
        // scope=1(global), type=1(messaging), name=welcome
        let mut input = Cursor::new("1\n1\nwelcome\n");
        let mut output = Vec::new();
        let mut answers_log = serde_json::Map::new();
        super::wizard_add_flow_with_io(dir.path(), &mut input, &mut output, &mut answers_log)
            .expect("wizard add flow");
        let path = dir.path().join("flows/global/messaging/welcome.ygtc");
        assert!(path.exists(), "expected flow file {}", path.display());
    }

    #[test]
    fn wizard_flow_summary_menu_updates_title_and_description() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        handle_update(
            UpdateArgs {
                flow_path: flow_path.clone(),
                flow_id: None,
                flow_type: None,
                schema_version: None,
                name: Some("Old Name".to_string()),
                description: Some("Old Description".to_string()),
                tags: None,
            },
            false,
        )
        .expect("seed metadata");

        // 2 flow list, 1 select flow, 1 summary, provide new name/desc, 7 save, then back/back/exit
        let input = Cursor::new("2\n1\n1\n2\nNew Name\nNew Description\n7\n\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard summary update");
        let output_text = String::from_utf8_lossy(&output);
        assert!(output_text.contains("Current name/title: Old Name"));
        assert!(output_text.contains("Current description: Old Description"));

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        assert_eq!(doc.title.as_deref(), Some("i18n:flow.main.title"));
        assert_eq!(
            doc.description.as_deref(),
            Some("i18n:flow.main.description")
        );
        let i18n_map: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join("i18n/en-GB.json")).expect("read i18n"),
        )
        .expect("parse i18n");
        assert_eq!(
            i18n_map.get("flow.main.title").and_then(|v| v.as_str()),
            Some("New Name")
        );
        assert_eq!(
            i18n_map
                .get("flow.main.description")
                .and_then(|v| v.as_str()),
            Some("New Description")
        );
    }

    #[test]
    fn wizard_flow_summary_shows_resolved_i18n_values() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/main.ygtc");
        if let Some(parent) = flow_path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(
            &flow_path,
            r#"id: main
type: messaging
schema_version: 2
title: i18n:flow.misc.title
description: i18n:flow.misc.description
nodes: {}
"#,
        )
        .expect("write flow");
        fs::create_dir_all(dir.path().join("i18n")).expect("create i18n dir");
        fs::write(
            dir.path().join("i18n/en-GB.json"),
            r#"{
  "flow.misc.title": "Best flow title",
  "flow.misc.description": "The best flow ever"
}"#,
        )
        .expect("write i18n");

        let mut input = Cursor::new("1\n");
        let mut output = Vec::new();
        let mut answers_log = serde_json::Map::new();
        super::wizard_edit_flow_summary_with_io(
            &flow_path,
            &mut input,
            &mut output,
            &mut answers_log,
        )
        .expect("summary view");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Best flow title"));
        assert!(rendered.contains("The best flow ever"));
        assert!(!rendered.contains("i18n:flow.misc.title"));
        assert!(!rendered.contains("i18n:flow.misc.description"));
    }

    #[test]
    fn wizard_changes_without_save_are_discarded() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: Some("Old Name".to_string()),
                description: Some("Old Description".to_string()),
                force: true,
            },
            false,
        )
        .expect("seed flow");

        // 2 flow list, 1 select flow, 1 summary edit, change fields, then back to main and exit without save.
        let input = Cursor::new("2\n1\n1\n2\nDraft Name\nDraft Description\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard summary update without save");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        assert_eq!(doc.title.as_deref(), Some("Old Name"));
        assert_eq!(doc.description.as_deref(), Some("Old Description"));
    }

    #[test]
    fn wizard_save_persists_changes() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        handle_update(
            UpdateArgs {
                flow_path: flow_path.clone(),
                flow_id: None,
                flow_type: None,
                schema_version: None,
                name: Some("Old Name".to_string()),
                description: Some("Old Description".to_string()),
                tags: None,
            },
            false,
        )
        .expect("seed metadata");

        // 2 flow list, 1 select flow, 1 summary edit, then 7 save, back to main and exit.
        let input = Cursor::new("2\n1\n1\n2\nNew Name\nNew Description\n7\n\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard summary update with save");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        assert_eq!(doc.title.as_deref(), Some("i18n:flow.main.title"));
        assert_eq!(
            doc.description.as_deref(),
            Some("i18n:flow.main.description")
        );
    }

    #[test]
    fn wizard_save_doctor_failure_does_not_persist() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");

        with_wizard_resolver_env(fixture_registry_resolver(), || {
            // Add step with fixture oci ref, try save (doctor should fail on sidecar ref validation), then exit.
            // No anchor prompt is shown when the flow has no steps.
            let input = Cursor::new("2\n1\n3\n2\noci://acme/widget:1\n2\n1\n7\n\n0\n0\n0\n");
            let mut output = Vec::new();
            super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard run");
            let rendered = String::from_utf8(output).expect("utf8");
            assert!(rendered.contains("Save blocked: doctor failed"));
            assert!(rendered.contains("Select operation"));
        });

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(flow_ir.nodes.is_empty(), "failed save should not persist");
    }

    #[test]
    fn wizard_save_default_answers_path_writes_file() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: Some("Old Name".to_string()),
                description: Some("Old Description".to_string()),
                force: true,
            },
            false,
        )
        .expect("seed flow");

        let input = Cursor::new("2\n1\n1\n2\nNew Name\nNew Description\n7\n\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard save");
        let answers_path = dir.path().join("answers.json");
        assert!(
            answers_path.exists(),
            "default answers file should be written"
        );
    }

    #[test]
    fn wizard_save_custom_answers_path_writes_file() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: Some("Old Name".to_string()),
                description: Some("Old Description".to_string()),
                force: true,
            },
            false,
        )
        .expect("seed flow");

        let input = Cursor::new("2\n1\n1\n2\nNew Name\nNew Description\n7\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_config(
            dir.path(),
            input,
            &mut output,
            super::WizardRunConfig {
                answers_file: Some(PathBuf::from("artifacts/answers-out.json")),
                dry_run: false,
            },
        )
        .expect("wizard save");
        let answers_path = dir.path().join("artifacts/answers-out.json");
        assert!(
            answers_path.exists(),
            "custom answers file should be written"
        );
    }

    #[test]
    fn wizard_main_menu_save_answers_option_writes_file() {
        let dir = tempdir().expect("temp dir");
        let input = Cursor::new("5\n\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard save answers");
        let answers_path = dir.path().join("answers.json");
        assert!(
            answers_path.exists(),
            "answers file should be written from main menu option 5"
        );
    }

    #[test]
    fn wizard_answers_export_and_reload_flow_matches_answers() {
        let dir = tempdir().expect("temp dir");
        let answers_rel = PathBuf::from("answers/replay.json");
        let answers_path = dir.path().join(&answers_rel);
        let flow_path = dir.path().join("flows/global/messaging/main.ygtc");
        write_two_step_flow(&flow_path);

        // Session 1: exercise menu flows, export answers via main menu 5, then save via 4.
        let input = Cursor::new(
            "2\n1\n\
             1\n2\nFlow One\nDesc One\n\
             2\n\
             4\ncancel\n\
             5\ncancel\n\
             0\nM\n\
             5\nanswers/replay.json\n\
             4\n\
             0\n",
        );
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard run 1");

        assert!(answers_path.exists(), "wizard answers export should exist");
        assert!(flow_path.exists(), "flow should be persisted after save");

        let answers_text = fs::read_to_string(&answers_path).expect("read answers file");
        let answers_json: serde_json::Value =
            serde_json::from_str(&answers_text).expect("parse answers json");
        let answers = answers_json.as_object().expect("answers object");
        assert_eq!(
            answers.get("wizard.answers.path").and_then(|v| v.as_str()),
            Some("answers/replay.json")
        );
        assert_eq!(
            answers.get("summary.name").and_then(|v| v.as_str()),
            Some("Flow One")
        );
        assert_eq!(
            answers.get("summary.description").and_then(|v| v.as_str()),
            Some("Desc One")
        );

        let before = fs::read_to_string(&flow_path).expect("read flow before reload");

        // Session 2: preload answers file and run main menu 4 (save).
        let input = Cursor::new("4\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_config(
            dir.path(),
            input,
            &mut output,
            super::WizardRunConfig {
                answers_file: Some(answers_rel.clone()),
                dry_run: false,
            },
        )
        .expect("wizard run 2");

        let after = fs::read_to_string(&flow_path).expect("read flow after reload");
        assert_eq!(before, after, "reload+save should preserve flow");

        let flow_doc = load_ygtc_from_path(&flow_path).expect("load saved flow");
        assert_eq!(flow_doc.id, "main");
        assert_eq!(flow_doc.flow_type, "messaging");
        assert_eq!(flow_doc.title.as_deref(), Some("i18n:flow.main.title"));
        assert_eq!(
            flow_doc.description.as_deref(),
            Some("i18n:flow.main.description")
        );
        let flow_ir = FlowIr::from_doc(flow_doc).expect("flow ir");
        assert!(
            flow_ir.nodes.contains_key("first") && flow_ir.nodes.contains_key("second"),
            "flow should preserve existing steps"
        );
    }

    #[test]
    fn wizard_dry_run_does_not_persist_flow_but_writes_answers_file() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: Some("Old Name".to_string()),
                description: Some("Old Description".to_string()),
                force: true,
            },
            false,
        )
        .expect("seed flow");

        let answers_path = dir.path().join("dry-run-answers.json");
        let input = Cursor::new("2\n1\n1\n2\nDry Name\nDry Description\n7\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_config(
            dir.path(),
            input,
            &mut output,
            super::WizardRunConfig {
                answers_file: Some(answers_path.clone()),
                dry_run: true,
            },
        )
        .expect("wizard dry-run save");

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        assert_eq!(doc.title.as_deref(), Some("Old Name"));
        assert_eq!(doc.description.as_deref(), Some("Old Description"));
        assert!(
            answers_path.exists(),
            "answers file should be written in dry-run"
        );
    }

    #[test]
    fn wizard_delete_flow_cancelled_keeps_file() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");
        let mut input = Cursor::new("1\n");
        let mut output = Vec::new();
        super::wizard_delete_flow_with_io(&flow_path, &mut input, &mut output)
            .expect("delete prompt");
        assert!(flow_path.exists(), "flow should remain after cancel");
    }

    #[test]
    fn wizard_menu_delete_flow_removes_file() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");

        // 2 flow list, 1 select flow, 6 delete, 2 yes, then 0 back to main, 4 save(default answers path), 0 exit
        let input = Cursor::new("2\n1\n6\n2\n0\n4\n\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard delete flow");
        assert!(!flow_path.exists(), "flow should be deleted");
    }

    fn write_two_step_flow(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(
            path,
            r#"id: main
type: messaging
schema_version: 2
nodes:
  first:
    op: {}
    routing:
    - to: second
  second:
    op: {}
    routing:
    - out: true
"#,
        )
        .expect("write two-step flow");
        let sidecar_path = super::sidecar_path_for_flow(path);
        fs::write(
            sidecar_path,
            r#"{
  "schema_version": 1,
  "flow": "welcome.ygtc",
  "nodes": {
    "first": {
      "source": { "kind": "repo", "ref": "repo://placeholder/first" }
    },
    "second": {
      "source": { "kind": "repo", "ref": "repo://placeholder/second" }
    }
  }
}"#,
        )
        .expect("write sidecar");
    }

    #[test]
    fn wizard_delete_step_removes_selected_node() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        let mut input = Cursor::new("first\n");
        let mut output = Vec::new();
        super::wizard_delete_step_with_io(&flow_path, &mut input, &mut output)
            .expect("delete step");
        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(!flow_ir.nodes.contains_key("first"));
        assert!(flow_ir.nodes.contains_key("second"));
    }

    #[test]
    fn wizard_menu_delete_step_path_removes_node() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        // 2 flow list, 1 select flow, 5 delete-step, first, 7 save, then 0(back) 0(main) 0(exit)
        let input = Cursor::new("2\n1\n5\nfirst\n7\n\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard menu delete step");
        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(!flow_ir.nodes.contains_key("first"));
        assert!(flow_ir.nodes.contains_key("second"));
    }

    #[test]
    fn wizard_menu_list_steps_shows_current_nodes() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        // 2 flow list, 1 select flow, 2 list-steps, then 0(back) 0(main) 0(exit)
        let input = Cursor::new("2\n1\n2\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output)
            .expect("wizard menu list steps");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Current steps"));
        assert!(rendered.contains("1. first"));
        assert!(rendered.contains("2. second"));
    }

    #[test]
    fn wizard_update_step_prompt_lists_current_nodes() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        let mut input = Cursor::new("cancel\n");
        let mut output = Vec::new();
        super::wizard_update_step_with_io(dir.path(), &flow_path, &mut input, &mut output)
            .expect("wizard update step");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("1. first"));
        assert!(rendered.contains("2. second"));
    }

    #[test]
    fn wizard_save_empty_flow_reports_clear_error() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        fs::create_dir_all(flow_path.parent().expect("parent")).expect("mkdirs");
        fs::write(
            &flow_path,
            r#"id: welcome
type: messaging
start: first
parameters: {}
tags: []
schema_version: 2
entrypoints: {}
nodes:
  first:
    op: {}
    routing:
    - out: true
"#,
        )
        .expect("write single-step flow");
        let sidecar_path = super::sidecar_path_for_flow(&flow_path);
        fs::write(
            sidecar_path,
            r#"{
  "schema_version": 1,
  "flow": "welcome.ygtc",
  "nodes": {
    "first": {
      "source": { "kind": "repo", "ref": "repo://placeholder/first" }
    }
  }
}"#,
        )
        .expect("write sidecar");

        // 2 flow list, 1 select flow, 5 delete-step, first, 7 save (fails), then back/main/exit
        let input = Cursor::new("2\n1\n5\nfirst\n7\n0\n0\n0\n");
        let mut output = Vec::new();
        super::run_wizard_menu_with_io(dir.path(), input, &mut output).expect("wizard run");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Save blocked: a flow must contain at least one step."));
    }

    #[test]
    fn doctor_lints_raw_summary_literals() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: Some("Raw Name".to_string()),
                description: Some("Raw Description".to_string()),
                force: true,
            },
            false,
        )
        .expect("seed flow");
        let errors = super::lint_i18n_tag_fields(&flow_path);
        assert!(!errors.is_empty(), "expected i18n tag lint errors");
    }

    fn with_wizard_resolver_env<F: FnOnce()>(resolver: String, run: F) {
        let _guard = env_test_lock();
        let previous = env::var("GREENTIC_FLOW_WIZARD_RESOLVER").ok();
        unsafe {
            env::set_var("GREENTIC_FLOW_WIZARD_RESOLVER", resolver);
        }
        run();
        if let Some(value) = previous {
            unsafe {
                env::set_var("GREENTIC_FLOW_WIZARD_RESOLVER", value);
            }
        } else {
            unsafe {
                env::remove_var("GREENTIC_FLOW_WIZARD_RESOLVER");
            }
        }
    }

    #[test]
    fn wizard_menu_add_step_remote_fixture() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");

        with_wizard_resolver_env(fixture_registry_resolver(), || {
            // 2 flow list, 1 flow, 3 add step, 2 remote, ref, 2 no-pin, 1 default mode, then back/main/exit.
            // No anchor prompt is shown when the flow has no steps.
            let input = Cursor::new("2\n1\n3\n2\noci://acme/widget:1\n2\n1\n0\n0\n0\n");
            let mut output = Vec::new();
            super::run_wizard_menu_with_io(dir.path(), input, &mut output)
                .expect("wizard menu add step");
            let rendered = String::from_utf8(output).expect("utf8");
            assert!(rendered.contains("Step added."));
        });

        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(
            flow_ir.nodes.is_empty(),
            "changes should not persist without save"
        );
    }

    #[test]
    fn wizard_add_step_anchor_no_nodes_defaults_to_auto_without_prompt() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");
        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        let mut input = Cursor::new("");
        let mut output = Vec::new();
        let selected =
            super::wizard_select_add_step_anchor_with_io(&flow_ir, &mut input, &mut output)
                .expect("select anchor");
        assert_eq!(selected, None);
        assert!(output.is_empty(), "no anchor prompt should be rendered");
    }

    #[test]
    fn wizard_add_step_anchor_multiple_nodes_uses_numbered_selection() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        write_two_step_flow(&flow_path);
        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        let mut input = Cursor::new("3\n");
        let mut output = Vec::new();
        let selected =
            super::wizard_select_add_step_anchor_with_io(&flow_ir, &mut input, &mut output)
                .expect("select anchor");
        assert_eq!(selected.as_deref(), Some("second"));
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("1. Auto"));
        assert!(rendered.contains("2. first"));
        assert!(rendered.contains("3. second"));
    }

    #[test]
    fn wizard_menu_update_step_remote_fixture() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");
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
                resolver: Some(resolver),
                pin: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        )
        .expect("add step");

        with_wizard_resolver_env(fixture_registry_resolver(), || {
            // 2 flow list, 1 flow, 4 update step, 1 step, 2 remote, ref, 1 default mode, 7 save, then back/main/exit.
            let input = Cursor::new("2\n1\n4\n1\n2\nrepo://acme/widget:1\n1\n7\n\n0\n0\n0\n");
            let mut output = Vec::new();
            super::run_wizard_menu_with_io(dir.path(), input, &mut output)
                .expect("wizard menu update step");
            let _rendered = String::from_utf8(output).expect("utf8");
        });
        let doc = load_ygtc_from_path(&flow_path).expect("load flow");
        let flow_ir = FlowIr::from_doc(doc).expect("flow ir");
        assert!(flow_ir.nodes.contains_key("widget"));
    }

    #[test]
    fn wizard_menu_update_step_twice_overwrites_answers_artifact() {
        let dir = tempdir().expect("temp dir");
        let flow_path = dir.path().join("flows/global/messaging/welcome.ygtc");
        handle_new(
            NewArgs {
                flow_path: flow_path.clone(),
                flow_id: "welcome".to_string(),
                flow_type: "messaging".to_string(),
                schema_version: 2,
                name: None,
                description: None,
                force: true,
            },
            false,
        )
        .expect("seed flow");
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
                resolver: Some(resolver),
                pin: false,
                allow_contract_change: false,
            },
            SchemaMode::Strict,
            OutputFormat::Human,
            false,
        )
        .expect("add step");

        with_wizard_resolver_env(fixture_registry_resolver(), || {
            // Update same step twice in default mode, then save.
            let input = Cursor::new(
                "2\n1\n4\n1\n2\nrepo://acme/widget:1\n1\n4\n1\n2\nrepo://acme/widget:1\n1\n7\n\n0\n0\n0\n",
            );
            let mut output = Vec::new();
            super::run_wizard_menu_with_io(dir.path(), input, &mut output)
                .expect("wizard menu update step twice");
            let rendered = String::from_utf8(output).expect("utf8");
            assert!(
                !rendered.contains("answers already exist"),
                "update-step should overwrite existing answers artifacts"
            );
        });
    }

    #[test]
    fn wizard_local_wasm_copy_places_file_under_pack_components() {
        let dir = tempdir().expect("temp dir");
        let external = dir.path().join("external-widget.wasm");
        fs::write(&external, b"\0asm....").expect("write wasm");
        let copied =
            super::copy_local_wasm_into_pack_components(dir.path(), &external).expect("copy wasm");
        assert!(copied.starts_with(dir.path().join("components")));
        assert!(copied.exists(), "copied wasm should exist");
        let src = fs::read(&external).expect("read source");
        let dst = fs::read(&copied).expect("read copied");
        assert_eq!(src, dst);
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

        let remove_err = handle_delete_step(
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
        .expect_err("remove mode should require explicit confirmation");
        assert!(
            remove_err.to_string().contains("Type REMOVE to confirm"),
            "unexpected remove confirmation error: {remove_err}"
        );

        handle_delete_step(
            DeleteStepArgs {
                component_id: None,
                flow_path: flow_path.clone(),
                step: Some("widget".to_string()),
                wizard_mode: None,
                answers: None,
                answers_file: None,
                answers_dir: None,
                overwrite_answers: false,
                reask: false,
                locale: None,
                interactive: false,
                component: None,
                local_wasm: None,
                distributor_url: None,
                auth_token: None,
                tenant: None,
                env: None,
                pack: None,
                component_version: None,
                abi_version: None,
                resolver: None,
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
        assert!(
            result.is_ok(),
            "canonical setup path without describe_cbor should skip hash drift checks"
        );
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

fn setup_contract_hint_from_error(err: &anyhow::Error) -> Option<&'static str> {
    let lower = err.to_string().to_ascii_lowercase();
    if lower.contains("missing exported component-qa instance")
        || lower.contains("missing exported component-descriptor instance")
        || lower.contains("missing exported component-qa.qa-spec function")
        || lower.contains("missing exported component-qa.apply-answers function")
    {
        return Some(
            "component is missing wizard setup exports (component-qa/component-descriptor). use a component built for setup flows, or add the step via explicit --operation/--payload without wizard mode",
        );
    }
    None
}

fn wrap_wizard_error(
    err: anyhow::Error,
    component_id: &str,
    op_fallback: &str,
    describe: Option<&greentic_types::schemas::component::v0_6_0::ComponentDescribe>,
) -> anyhow::Error {
    let op = wizard_op_from_error(&err, op_fallback);
    if let Some(hint) = setup_contract_hint_from_error(&err) {
        return err.context(format!(
            "component '{component_id}' operation '{op}' failed: {hint}"
        ));
    }
    if let Some(cap) = capability_hint_from_error(&err, describe) {
        err.context(format!(
            "component '{component_id}' operation '{op}' failed due to denied host ref; requested capability '{cap}'. hint: grant capability {cap} to this component"
        ))
    } else {
        err.context(format!(
            "component '{component_id}' operation '{op}' failed"
        ))
    }
}

fn ensure_wizard_config_not_error(
    component_id: &str,
    mode: wizard_ops::WizardMode,
    config_json: &serde_json::Value,
) -> Result<()> {
    let Some(error_obj) = config_json.get("error").and_then(|value| value.as_object()) else {
        return Ok(());
    };
    let code = error_obj
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("UNKNOWN");
    let message = error_obj
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("component returned an error payload");
    let details = error_obj
        .get("details")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if details.is_empty() {
        anyhow::bail!(
            "component '{component_id}' setup failed in mode '{}': {code}: {message}",
            mode.as_str()
        );
    }
    anyhow::bail!(
        "component '{component_id}' setup failed in mode '{}': {code}: {message} ({details})",
        mode.as_str()
    );
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

fn confirm_remove_mode(interactive: bool) -> Result<()> {
    if !interactive {
        anyhow::bail!("remove mode requires interactive confirmation: Type REMOVE to confirm");
    }
    let mut stdout = io::stdout().lock();
    write!(stdout, "Type REMOVE to confirm: ").context("write confirmation prompt")?;
    stdout.flush().context("flush confirmation prompt")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read remove confirmation")?;
    if line.trim() != "REMOVE" {
        anyhow::bail!("remove cancelled: confirmation did not match 'REMOVE'");
    }
    Ok(())
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
    handle_add_step_with_qa_io(args, schema_mode, format, backup, None)
}

fn handle_add_step_with_qa_io(
    args: AddStepArgs,
    schema_mode: SchemaMode,
    format: OutputFormat,
    backup: bool,
    qa_io: Option<&mut QaInteractiveIo<'_>>,
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
                descriptor: None,
                qa_spec_cbor: fixture.qa_spec_cbor.clone(),
                answers_schema_cbor: None,
            }
        } else {
            wizard_ops::fetch_wizard_spec(&resolved.wasm_bytes, wizard_mode)
                .map_err(|err| wrap_wizard_error(err, &component_identity, "describe", None))?
        };
        let qa_spec = wizard_ops::decode_component_qa_spec(&spec.qa_spec_cbor, wizard_mode)?;
        let (mut catalog, locale) = default_i18n_catalog(args.locale.as_deref());
        merge_component_i18n_catalog(&mut catalog, &locale, &args.flow_path, &resolved.source);

        let mut answers = parse_answers_map(args.answers.as_deref(), args.answers_file.as_deref())?;
        wizard_ops::merge_default_answers(&qa_spec, &mut answers);
        if args.interactive && matches!(wizard_mode, wizard_ops::WizardMode::Default) {
            seed_optional_answers_for_default_setup(&qa_spec, &mut answers);
        }
        if !qa_spec.questions.is_empty() {
            qa_runner::warn_unknown_keys(&answers, &qa_spec, &catalog, &locale);
            println!(
                "{}",
                wizard_header(&component_identity, wizard_mode.as_str())
            );
            answers = run_component_qa_with_qa_lib(
                &qa_spec,
                &catalog,
                &locale,
                answers,
                args.interactive,
                qa_io,
            )?;
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
            .map_err(|err| wrap_wizard_error(err, &component_identity, "apply-answers", None))?
        };
        let operation_id = args.operation.clone().unwrap_or_else(|| "run".to_string());
        let config_json = wizard_ops::cbor_to_json(&config_cbor)?;
        ensure_wizard_config_not_error(&component_identity, wizard_mode, &config_json)?;

        let operation = operation_id;
        let contract_meta = spec
            .descriptor
            .as_ref()
            .map(|descriptor| derive_contract_meta_from_descriptor(descriptor, &operation))
            .transpose()?
            .map(|(_, meta)| meta);
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
            contract_meta.as_ref(),
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
    handle_update_step_with_qa_io(args, schema_mode, format, backup, None)
}

fn handle_update_step_with_qa_io(
    args: UpdateStepArgs,
    schema_mode: SchemaMode,
    format: OutputFormat,
    backup: bool,
    qa_io: Option<&mut QaInteractiveIo<'_>>,
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
                descriptor: None,
                qa_spec_cbor: fixture.qa_spec_cbor.clone(),
                answers_schema_cbor: None,
            }
        } else {
            wizard_ops::fetch_wizard_spec(&resolved.wasm_bytes, wizard_mode)
                .map_err(|err| wrap_wizard_error(err, &component_identity, "describe", None))?
        };
        let qa_spec = wizard_ops::decode_component_qa_spec(&spec.qa_spec_cbor, wizard_mode)?;
        let (mut catalog, locale) = default_i18n_catalog(args.locale.as_deref());
        merge_component_i18n_catalog(&mut catalog, &locale, &args.flow_path, &resolved.source);

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
        if args.interactive && matches!(wizard_mode, wizard_ops::WizardMode::Default) {
            seed_optional_answers_for_default_setup(&qa_spec, &mut answers);
        }
        if !qa_spec.questions.is_empty() {
            qa_runner::warn_unknown_keys(&answers, &qa_spec, &catalog, &locale);
            println!(
                "{}",
                wizard_header(&component_identity, wizard_mode.as_str())
            );
            answers = run_component_qa_with_qa_lib(
                &qa_spec,
                &catalog,
                &locale,
                answers,
                args.interactive,
                qa_io,
            )?;
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
            .map_err(|err| wrap_wizard_error(err, &component_identity, "apply-answers", None))?
        };
        let mut new_operation = node.operation.clone();
        if let Some(op) = args.operation.clone() {
            new_operation = op;
        }
        let contract_meta = spec
            .descriptor
            .as_ref()
            .map(|descriptor| derive_contract_meta_from_descriptor(descriptor, &new_operation))
            .transpose()?
            .map(|(_, meta)| meta);
        let config_json = wizard_ops::cbor_to_json(&config_cbor)?;
        ensure_wizard_config_not_error(&component_identity, wizard_mode, &config_json)?;
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
            contract_meta.as_ref(),
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
        if matches!(wizard_mode, wizard_ops::WizardMode::Remove) {
            confirm_remove_mode(args.interactive)?;
        }
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
                descriptor: None,
                qa_spec_cbor: fixture.qa_spec_cbor.clone(),
                answers_schema_cbor: None,
            }
        } else {
            wizard_ops::fetch_wizard_spec(&resolved.wasm_bytes, wizard_mode)
                .map_err(|err| wrap_wizard_error(err, &component_identity, "describe", None))?
        };
        let qa_spec = wizard_ops::decode_component_qa_spec(&spec.qa_spec_cbor, wizard_mode)?;
        let (mut catalog, locale) = default_i18n_catalog(args.locale.as_deref());
        merge_component_i18n_catalog(&mut catalog, &locale, &args.flow_path, &resolved.source);

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
            qa_runner::warn_unknown_keys(&answers, &qa_spec, &catalog, &locale);
            println!(
                "{}",
                wizard_header(&component_identity, wizard_mode.as_str())
            );
            answers = run_component_qa_with_qa_lib(
                &qa_spec,
                &catalog,
                &locale,
                answers,
                args.interactive,
                None,
            )?;
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
            .map_err(|err| wrap_wizard_error(err, &component_identity, "apply-answers", None))?;
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

fn seed_optional_answers_for_default_setup(
    qa_spec: &greentic_types::schemas::component::v0_6_0::ComponentQaSpec,
    answers: &mut QuestionAnswers,
) {
    for question in &qa_spec.questions {
        if question.required || question.default.is_some() {
            continue;
        }
        answers
            .entry(question.id.clone())
            .or_insert(serde_json::Value::Null);
    }
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
        let abi = wizard_ops::WizardAbi::V6;
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
        let _ = fs::read_to_string(&abi_path)
            .with_context(|| format!("read {}", abi_path.display()))?;
        wizard_ops::WizardAbi::V6
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
