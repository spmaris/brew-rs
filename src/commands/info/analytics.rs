use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::Config;
use crate::ui::DnfStyleFormatter;

use super::request::{ANALYTICS_PERIODS, Kind};
use super::response::{AnalyticsEntry, AnalyticsResponse};
use super::source::Metadata;

pub(super) fn print_analytics(
    config: &Config,
    metadata: &Metadata,
    period: &str,
    response: &AnalyticsResponse,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(config);
    println!(
        "{}",
        formatter.header(&format!(
            "Analytics for {} ({})",
            metadata.name,
            metadata.kind.label()
        ))
    );
    println!(
        "{}",
        formatter.field(
            "Category",
            &category_display(&response.category, metadata.kind)
        )
    );
    println!(
        "{}",
        formatter.field("Period", &period_description(period, response))
    );
    if response.total_items > 0 {
        println!(
            "{}",
            formatter.field(
                "Tracked",
                &format!("{} packages", format_count(response.total_items))
            )
        );
    }

    let Some(entries) = response.formulae.get(&metadata.name) else {
        println!();
        println!("{}", no_analytics_message(metadata, period, response));
        return Ok(());
    };
    let breakdown = analytics_breakdown(&metadata.name, entries)?;
    if breakdown.is_empty() {
        println!();
        println!("{}", no_analytics_message(metadata, period, response));
        return Ok(());
    }

    let canonical = breakdown
        .iter()
        .find(|(label, _)| label == &metadata.name)
        .map(|(_, count)| *count)
        .unwrap_or_else(|| breakdown.iter().map(|(_, count)| *count).sum());
    let installs = if response.total_count == 0 {
        format!(
            "{} installs (tap total unavailable)",
            format_count(canonical)
        )
    } else {
        match format_percent(canonical, response.total_count) {
            Some(percent) => format!(
                "{} of {} installs ({percent})",
                format_count(canonical),
                format_count(response.total_count)
            ),
            None => format!("{} installs", format_count(canonical)),
        }
    };
    println!("{}", formatter.field("Installs", &installs));

    if breakdown.len() > 1 {
        println!();
        let rows: Vec<Vec<String>> = breakdown
            .iter()
            .map(|(label, count)| vec![label.clone(), format_count(*count)])
            .collect();
        println!(
            "{}",
            formatter.table("Breakdown", &["Variant", "Installs"], &rows)
        );
    }
    Ok(())
}

pub(super) fn no_analytics_message(
    metadata: &Metadata,
    period: &str,
    response: &AnalyticsResponse,
) -> String {
    let category = category_display(&response.category, metadata.kind);
    let range = period_range(response);
    format!(
        "No {category} analytics recorded for '{}' in {period}{range}.",
        metadata.name
    )
}

/// The effective analytics category, falling back to the namespace's own.
pub(super) fn category_display(category: &str, kind: Kind) -> String {
    let category = category.trim();
    if category.is_empty() {
        kind.analytics_category().to_string()
    } else {
        category.to_string()
    }
}

pub(super) fn period_description(period: &str, response: &AnalyticsResponse) -> String {
    let range = period_range(response);
    if range.is_empty() {
        period.to_string()
    } else {
        format!("{period}{range}")
    }
}

pub(super) fn period_range(response: &AnalyticsResponse) -> String {
    let start = response.start_date.trim();
    let end = response.end_date.trim();
    if start.is_empty() || end.is_empty() {
        String::new()
    } else {
        format!(" ({start} to {end})")
    }
}

pub(super) fn analytics_breakdown(
    name: &str,
    entries: &[AnalyticsEntry],
) -> Result<Vec<(String, u64)>> {
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let label = entry
            .formula
            .as_deref()
            .or(entry.cask.as_deref())
            .with_context(|| {
                format!("Homebrew analytics entry for '{name}' has no formula or cask label")
            })?;
        let count = parse_count(&entry.count).with_context(|| {
            format!("Homebrew analytics entry '{label}' has an invalid install count")
        })?;
        rows.push((label.to_string(), count));
    }
    Ok(rows)
}

/// Inline analytics published with a formula or cask payload.
///
/// `install` maps a period (`30d`, `90d`, `365d`) to the counts of the exact
/// identifier and of option/HEAD variants (for example `jq --HEAD`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct InlineAnalytics {
    pub(super) install: BTreeMap<String, BTreeMap<String, u64>>,
}

pub(super) fn inline_analytics_rows(name: &str, analytics: &InlineAnalytics) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for period in sorted_periods(&analytics.install) {
        let Some(counts) = analytics.install.get(period) else {
            continue;
        };
        let Some((canonical, variants)) = inline_counts(name, counts) else {
            continue;
        };
        rows.push(vec![
            period.to_string(),
            format_count(canonical),
            if variants == 0 {
                String::new()
            } else {
                format_count(variants)
            },
        ]);
    }
    rows
}

pub(super) fn inline_counts(name: &str, counts: &BTreeMap<String, u64>) -> Option<(u64, u64)> {
    let mut canonical = None;
    let mut variants = 0u64;
    let mut matched = false;
    for (label, count) in counts {
        if label == name {
            canonical = Some(*count);
            matched = true;
        } else if label
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(' '))
        {
            variants = variants.saturating_add(*count);
            matched = true;
        }
    }
    if matched {
        Some((canonical.unwrap_or(0), variants))
    } else {
        None
    }
}

pub(super) fn sorted_periods(install: &BTreeMap<String, BTreeMap<String, u64>>) -> Vec<&str> {
    let mut periods: Vec<&str> = install.keys().map(String::as_str).collect();
    periods.sort_by(|left, right| {
        period_rank(left)
            .cmp(&period_rank(right))
            .then_with(|| left.cmp(right))
    });
    periods
}

pub(super) fn period_rank(period: &str) -> usize {
    ANALYTICS_PERIODS
        .iter()
        .position(|known| *known == period)
        .unwrap_or(ANALYTICS_PERIODS.len())
}

pub(super) fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

pub(super) fn parse_count(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("count is empty");
    }
    let parts: Vec<&str> = raw.split(',').collect();
    let Some((first, rest)) = parts.split_first() else {
        bail!("invalid count '{raw}'");
    };
    if first.is_empty() || first.len() > 3 || !first.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid count '{raw}'");
    }
    for part in rest {
        if part.len() != 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("invalid count '{raw}'");
        }
    }
    let digits: String = parts.concat();
    digits
        .parse::<u64>()
        .with_context(|| format!("count '{raw}' is above the supported maximum"))
}

pub(super) fn format_percent(part: u64, total: u64) -> Option<String> {
    if total == 0 {
        return None;
    }
    let percent = part as f64 * 100.0 / total as f64;
    Some(if percent < 0.01 {
        format!("{percent:.4}%")
    } else {
        format!("{percent:.2}%")
    })
}
