use anyhow::{Context, Result};
use clap::Parser;
use greentic_flow::{
    lint::{lint_builtin_rules, lint_with_registry},
    loader::load_ygtc_from_str,
    registry::AdapterCatalog,
    to_ir,
};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process,
};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Validate YGTC flows against the schema and optional adapter registry."
)]
struct Cli {
    /// Path to the flow schema JSON file.
    #[arg(long, default_value = "schemas/ygtc.flow.schema.json")]
    schema: PathBuf,
    /// Optional adapter catalog used for adapter_resolvable linting.
    #[arg(long)]
    registry: Option<PathBuf>,
    /// Flow files or directories to lint.
    #[arg(required = true)]
    targets: Vec<PathBuf>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let registry = if let Some(path) = &cli.registry {
        Some(AdapterCatalog::load_from_file(path)?)
    } else {
        None
    };

    let mut failures = 0usize;
    for target in cli.targets {
        lint_path(&target, &cli.schema, registry.as_ref(), &mut failures)?;
    }

    if failures == 0 {
        println!("All flows valid");
        Ok(())
    } else {
        Err(anyhow::anyhow!("{failures} flow(s) failed validation"))
    }
}

fn lint_path(
    path: &Path,
    schema: &Path,
    registry: Option<&AdapterCatalog>,
    failures: &mut usize,
) -> Result<()> {
    if path.is_file() {
        lint_file(path, schema, registry, failures)?;
    } else if path.is_dir() {
        let entries = fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?;
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read directory entry in {}", path.display()))?;
            lint_path(&entry.path(), schema, registry, failures)?;
        }
    }
    Ok(())
}

fn lint_file(
    path: &Path,
    schema: &Path,
    registry: Option<&AdapterCatalog>,
    failures: &mut usize,
) -> Result<()> {
    if path.extension() != Some(OsStr::new("ygtc")) {
        return Ok(());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    match load_ygtc_from_str(&content, schema) {
        Ok(flow) => match to_ir(flow) {
            Ok(ir) => {
                let errors = if let Some(cat) = registry {
                    lint_with_registry(&ir, cat)
                } else {
                    lint_builtin_rules(&ir)
                };
                if errors.is_empty() {
                    println!("OK  {} ({})", path.display(), ir.id);
                } else {
                    *failures += 1;
                    eprintln!("ERR {}:", path.display());
                    for err in errors {
                        eprintln!("  {err}");
                    }
                }
            }
            Err(err) => {
                *failures += 1;
                eprintln!("ERR {}: {err}", path.display());
            }
        },
        Err(err) => {
            *failures += 1;
            eprintln!("ERR {}: {err}", path.display());
        }
    }
    Ok(())
}
