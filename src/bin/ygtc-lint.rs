use greentic_flow::loader::load_ygtc_from_str;
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process,
};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mut schema_path = PathBuf::from("schemas/ygtc.flow.schema.json");
    let mut targets: Vec<PathBuf> = Vec::new();

    while let Some(arg) = args.next() {
        if arg == "--schema" {
            let value = args
                .next()
                .ok_or_else(|| "--schema requires a path".to_string())?;
            schema_path = PathBuf::from(value);
            continue;
        }
        if arg == "--help" || arg == "-h" {
            print_usage();
            return Ok(());
        }
        targets.push(PathBuf::from(arg));
    }

    if targets.is_empty() {
        print_usage();
        return Err("no flow paths provided".into());
    }

    let mut failures = 0usize;
    for target in targets {
        lint_path(&target, &schema_path, &mut failures)?;
    }

    if failures == 0 {
        println!("All flows valid");
        Ok(())
    } else {
        Err(format!("{failures} flow(s) failed validation"))
    }
}

fn lint_path(path: &Path, schema: &Path, failures: &mut usize) -> Result<(), String> {
    if path.is_file() {
        lint_file(path, schema, failures)?;
    } else if path.is_dir() {
        let entries =
            fs::read_dir(path).map_err(|e| format!("failed to read directory {path:?}: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
            lint_path(&entry.path(), schema, failures)?;
        }
    }
    Ok(())
}

fn lint_file(path: &Path, schema: &Path, failures: &mut usize) -> Result<(), String> {
    if path.extension() != Some(OsStr::new("ygtc")) {
        return Ok(());
    }

    let content = fs::read_to_string(path).map_err(|e| format!("failed to read {path:?}: {e}"))?;

    match load_ygtc_from_str(&content, schema) {
        Ok(flow) => {
            println!("OK  {} ({})", path.display(), flow.id);
        }
        Err(err) => {
            *failures += 1;
            eprintln!("ERR {}: {err}", path.display());
        }
    }
    Ok(())
}

fn print_usage() {
    eprintln!("Usage: ygtc-lint [--schema path] <flow files or directories>...");
}
