use std::collections::HashSet;

fn slugify(raw: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in raw.chars().enumerate() {
        let safe = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch
        } else {
            '_'
        };
        if idx == 0 && !(safe.is_ascii_alphabetic() || safe == '_') {
            out.push('_');
        }
        out.push(safe);
    }
    if out.is_empty() { "_".to_string() } else { out }
}

fn is_placeholder(hint: Option<&str>) -> bool {
    match hint {
        None => true,
        Some(h) => is_placeholder_value(h),
    }
}

pub fn is_placeholder_value(hint: &str) -> bool {
    let trimmed = hint.trim();
    trimmed.is_empty()
        || matches!(
            trimmed,
            "STEP" | "NODE" | "COMPONENT_STEP" | "INSERT_NODE" | "NEW_NODE"
        )
}

pub fn generate_node_id<'a, I: Iterator<Item = &'a str>>(
    hint: Option<&str>,
    anchor: &str,
    existing: I,
) -> String {
    let used: HashSet<String> = existing.map(|s| s.to_string()).collect();
    let base = if !is_placeholder(hint) {
        let slug = slugify(hint.unwrap());
        if !slug.is_empty() {
            slug
        } else {
            "node".to_string()
        }
    } else {
        let mut parts = vec!["node".to_string()];
        parts.push("after".to_string());
        parts.push(slugify(anchor));
        parts.join("__")
    };

    if !used.contains(&base) && !is_placeholder(hint) {
        return base;
    }

    let mut candidate = base.clone();
    let mut idx = 2usize;
    while used.contains(&candidate) {
        candidate = format!("{base}__{idx}");
        idx += 1;
    }
    candidate
}
