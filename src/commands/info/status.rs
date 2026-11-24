use super::response::{CaskResponse, FormulaResponse};

pub(super) fn formula_versions(formula: &FormulaResponse) -> String {
    let revision = formula.revision.unwrap_or(0);
    let mut parts = Vec::new();
    match formula
        .versions
        .stable
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
    {
        Some(stable) => parts.push(format!("stable {}", with_revision(stable, revision))),
        None => parts.push("stable none".to_string()),
    }
    if let Some(head) = formula
        .versions
        .head
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
    {
        parts.push(format!("head {head}"));
    }
    parts.join(", ")
}

pub(super) fn cask_version(cask: &CaskResponse) -> String {
    let version = cask.version.trim();
    let version = if version.is_empty() {
        "unknown".to_string()
    } else {
        version.to_string()
    };
    if cask.auto_updates == Some(true) {
        format!("{version} (auto-updating)")
    } else {
        version
    }
}

pub(super) fn with_revision(version: &str, revision: u32) -> String {
    if revision == 0 {
        version.to_string()
    } else {
        format!("{version}_{revision}")
    }
}

pub(super) fn keg_only_value(formula: &FormulaResponse) -> String {
    match formula.keg_only_reason.as_ref() {
        Some(reason) if !reason.reason.trim().is_empty() => {
            let reason_text = reason.reason.trim();
            if reason.explanation.trim().is_empty() {
                format!("yes ({reason_text})")
            } else {
                format!("yes ({reason_text}: {})", reason.explanation.trim())
            }
        }
        _ => "yes".to_string(),
    }
}

pub(super) fn formula_status_messages(formula: &FormulaResponse, name: &str) -> Vec<String> {
    let mut messages = Vec::new();
    if formula.disabled {
        messages.push(status_message(
            name,
            "disabled",
            &formula.disable_date,
            &formula.disable_reason,
            replacement_text(
                &formula.disable_replacement_formula,
                &formula.disable_replacement_cask,
            ),
        ));
    } else if formula.deprecated {
        messages.push(status_message(
            name,
            "deprecated",
            &formula.deprecation_date,
            &formula.deprecation_reason,
            replacement_text(
                &formula.deprecation_replacement_formula,
                &formula.deprecation_replacement_cask,
            ),
        ));
    }
    messages
}

pub(super) fn cask_status_messages(cask: &CaskResponse, token: &str) -> Vec<String> {
    let mut messages = Vec::new();
    if cask.disabled {
        messages.push(status_message(
            token,
            "disabled",
            &cask.disable_date,
            &cask.disable_reason,
            replacement_text(
                &cask.disable_replacement_formula,
                &cask.disable_replacement_cask,
            ),
        ));
    } else if cask.deprecated {
        messages.push(status_message(
            token,
            "deprecated",
            &cask.deprecation_date,
            &cask.deprecation_reason,
            replacement_text(
                &cask.deprecation_replacement_formula,
                &cask.deprecation_replacement_cask,
            ),
        ));
    }
    messages
}

pub(super) fn status_message(
    name: &str,
    state: &str,
    date: &Option<String>,
    reason: &Option<String>,
    replacement: Option<String>,
) -> String {
    let mut message = format!("{name} is {state}");
    if let Some(date) = non_empty(date.as_deref()) {
        message.push_str(&format!(" since {date}"));
    }
    if let Some(reason) = non_empty(reason.as_deref()) {
        message.push_str(&format!(": {reason}"));
    }
    if let Some(replacement) = replacement {
        message.push_str(&format!(" (replacement: {replacement})"));
    }
    message
}

pub(super) fn replacement_text(formula: &Option<String>, cask: &Option<String>) -> Option<String> {
    match (non_empty(formula.as_deref()), non_empty(cask.as_deref())) {
        (Some(formula), Some(cask)) => Some(format!("{formula} (formula) or {cask} (cask)")),
        (Some(formula), None) => Some(formula.to_string()),
        (None, Some(cask)) => Some(cask.to_string()),
        (None, None) => None,
    }
}

pub(super) fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(super) fn optional_text<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    non_empty(value).unwrap_or(fallback)
}

pub(super) fn tap_or(tap: &str, fallback: &str) -> String {
    non_empty(Some(tap))
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) fn cask_display_name(cask: &CaskResponse) -> Option<String> {
    let names: Vec<&str> = cask
        .name
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() || (names.len() == 1 && names[0] == cask.token) {
        return None;
    }
    Some(names.join(", "))
}
