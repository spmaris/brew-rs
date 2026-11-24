use super::response::FormulaResponse;

pub(super) fn formula_dependency_rows(formula: &FormulaResponse) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    push_dependency_rows(&mut rows, "Runtime", &formula.dependencies);
    push_dependency_rows(&mut rows, "Recommended", &formula.recommended_dependencies);
    push_dependency_rows(&mut rows, "Optional", &formula.optional_dependencies);
    push_dependency_rows(&mut rows, "Build", &formula.build_dependencies);
    for value in &formula.uses_from_macos {
        match value {
            serde_json::Value::String(name) => {
                rows.push(vec!["macOS".to_string(), name.clone()]);
            }
            serde_json::Value::Object(scopes) => {
                for (name, scope) in scopes {
                    let scope = render_value(scope);
                    let text = if scope.is_empty() || scope == "runtime" {
                        name.clone()
                    } else {
                        format!("{name} ({scope})")
                    };
                    rows.push(vec!["macOS".to_string(), text]);
                }
            }
            other => rows.push(vec!["macOS".to_string(), render_value(other)]),
        }
    }
    rows
}

pub(super) fn push_dependency_rows(rows: &mut Vec<Vec<String>>, kind: &str, names: &[String]) {
    for name in names {
        rows.push(vec![kind.to_string(), name.clone()]);
    }
}

pub(super) fn cask_dependency_rows(depends_on: &serde_json::Value) -> Vec<Vec<String>> {
    let Some(object) = depends_on.as_object() else {
        return Vec::new();
    };
    let mut rows = Vec::new();

    if let Some(macos) = object.get("macos") {
        match macos {
            // An empty macOS map means "no constraint", not a dependency.
            serde_json::Value::Object(bounds) => {
                for (operator, versions) in bounds {
                    rows.push(vec![
                        "macOS".to_string(),
                        format!("{operator} {}", render_value(versions)),
                    ]);
                }
            }
            other => rows.push(vec!["macOS".to_string(), render_value(other)]),
        }
    }
    if let Some(arch) = object.get("arch") {
        rows.push(vec!["Architecture".to_string(), render_value(arch)]);
    }
    for (key, label) in [("formula", "Formula"), ("cask", "Cask")] {
        if let Some(value) = object.get(key) {
            for name in list_values(value) {
                rows.push(vec![label.to_string(), name]);
            }
        }
    }
    for (key, value) in object {
        if matches!(key.as_str(), "macos" | "arch" | "formula" | "cask") {
            continue;
        }
        rows.push(vec![humanize(key), render_value(value)]);
    }
    rows
}

pub(super) fn cask_conflicts_value(conflicts: &serde_json::Value) -> Option<String> {
    let object = conflicts.as_object()?;
    let mut parts = Vec::new();
    for (kind, names) in object {
        let names = list_values(names);
        if !names.is_empty() {
            parts.push(format!("{} ({kind})", names.join(", ")));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

pub(super) fn list_values(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(items) => items.iter().map(render_value).collect(),
        serde_json::Value::String(_) => vec![render_value(value)],
        serde_json::Value::Null => Vec::new(),
        other => vec![render_value(other)],
    }
}

pub(super) fn render_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Bool(true) => "yes".to_string(),
        serde_json::Value::Bool(false) => "no".to_string(),
        serde_json::Value::Null => "unknown".to_string(),
        other => other.to_string(),
    }
}

pub(super) fn humanize(key: &str) -> String {
    let mut output = String::with_capacity(key.len());
    for (index, part) in key.split('_').filter(|part| !part.is_empty()).enumerate() {
        if index > 0 {
            output.push(' ');
        }
        let mut characters = part.chars();
        if let Some(first) = characters.next() {
            output.extend(first.to_uppercase());
            output.push_str(characters.as_str());
        }
    }
    if output.is_empty() {
        key.to_string()
    } else {
        output
    }
}

pub(super) fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

pub(super) fn clean_block(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() && character != '\n' {
                ' '
            } else {
                character
            }
        })
        .collect()
}
