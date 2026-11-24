use serde_json::Value;

use crate::domain::plan;

use super::source::FormulaPayload;

/// Excluded dependency names shown per category for the queried formula.
pub(super) const MAX_EXCLUDED_NAMES_PER_KIND: usize = 8;

/// Categories of dependencies this query does not expand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExcludedKind {
    Build,
    Test,
    Optional,
    Recommended,
    Macos,
}

impl ExcludedKind {
    const ALL: [Self; 5] = [
        Self::Build,
        Self::Test,
        Self::Optional,
        Self::Recommended,
        Self::Macos,
    ];

    /// Stable label used in output.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
            Self::Optional => "optional",
            Self::Recommended => "recommended",
            Self::Macos => "uses_from_macos",
        }
    }

    /// Position in [`ExcludedCounts`].
    pub(super) fn index(self) -> usize {
        match self {
            Self::Build => 0,
            Self::Test => 1,
            Self::Optional => 2,
            Self::Recommended => 3,
            Self::Macos => 4,
        }
    }

    /// The raw payload list for this category.
    pub(super) fn values(self, payload: &FormulaPayload) -> &[Value] {
        match self {
            Self::Build => &payload.build_dependencies,
            Self::Test => &payload.test_dependencies,
            Self::Optional => &payload.optional_dependencies,
            Self::Recommended => &payload.recommended_dependencies,
            Self::Macos => &payload.uses_from_macos,
        }
    }
}

/// Per-category counts of dependencies outside the runtime graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ExcludedCounts([usize; 5]);

impl ExcludedCounts {
    pub(super) fn from_payload(payload: &FormulaPayload) -> Self {
        let mut counts = [0usize; 5];
        for kind in ExcludedKind::ALL {
            counts[kind.index()] = kind.values(payload).len();
        }
        Self(counts)
    }

    pub(super) fn get(self, kind: ExcludedKind) -> usize {
        self.0[kind.index()]
    }

    pub(super) fn total(self) -> usize {
        self.0.iter().sum()
    }
}

/// One named dependency outside the runtime graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExcludedEntry {
    pub(super) kind: ExcludedKind,
    pub(super) name: String,
}

/// Collect validated excluded dependency names, bounded per category.
pub(super) fn excluded_entries(payload: &FormulaPayload) -> Vec<ExcludedEntry> {
    let mut entries = Vec::new();
    for kind in ExcludedKind::ALL {
        for name in value_names(kind.values(payload))
            .into_iter()
            .take(MAX_EXCLUDED_NAMES_PER_KIND)
        {
            entries.push(ExcludedEntry { kind, name });
        }
    }
    entries
}

/// Extract validated dependency identifiers from loosely typed API entries.
///
/// Entries are either `"name"` strings or objects with a `"name"` field. Every
/// identifier is validated by the planner's target parser, and anything it
/// rejects is silently left out of the display list; the category count still
/// reflects the raw length.
pub(super) fn value_names(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| match value {
            Value::String(name) => Some(name.as_str()),
            Value::Object(object) => object.get("name").and_then(Value::as_str),
            _ => None,
        })
        .filter_map(|name| plan::parse_target(name).ok().map(str::to_owned))
        .collect()
}

/// Render the excluded scope as one line per non-empty category.
pub(super) fn format_excluded(counts: ExcludedCounts, entries: &[ExcludedEntry]) -> String {
    let mut lines = Vec::new();
    for kind in ExcludedKind::ALL {
        let total = counts.get(kind);
        if total == 0 {
            continue;
        }
        let names: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.kind == kind)
            .map(|entry| entry.name.as_str())
            .collect();
        if names.is_empty() {
            lines.push(format!("{}: {total}", kind.label()));
            continue;
        }
        let hidden = total.saturating_sub(names.len());
        if hidden == 0 {
            lines.push(format!("{}: {}", kind.label(), names.join(", ")));
        } else {
            lines.push(format!(
                "{}: {} (+{hidden} more)",
                kind.label(),
                names.join(", ")
            ));
        }
    }
    if lines.is_empty() {
        "none".to_string()
    } else {
        lines.join("\n")
    }
}
