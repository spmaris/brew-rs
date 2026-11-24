//! The edit plan: mapper results, expected targets and marker checks.

use std::collections::HashMap;
use std::ops::Range;

use super::super::error::RelocateError;
use super::constants::{MARKER, MARKER_TEXT};
use super::error::MachoError;
use super::model::{Kind, Parsed, PathKind};
use super::parse::validate_name;

/// One planned `install_name_tool` edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Change {
    pub(super) kind: Kind,
    pub(super) old: String,
    pub(super) new: String,
}

/// The complete edit plan, including the expected value of every path-bearing
/// command (indexed like [`Parsed::commands`]).
#[derive(Debug)]
pub(super) struct Plan {
    pub(super) changes: Vec<Change>,
    pub(super) expected: Vec<Option<String>>,
}

impl Plan {
    pub(super) fn arguments(&self) -> Vec<String> {
        let mut arguments = Vec::with_capacity(self.changes.len().saturating_mul(3));
        for change in &self.changes {
            match change.kind {
                Kind::IdDylib => {
                    arguments.push("-id".to_owned());
                    arguments.push(change.new.clone());
                }
                Kind::Rpath => {
                    arguments.push("-rpath".to_owned());
                    arguments.push(change.old.clone());
                    arguments.push(change.new.clone());
                }
                _ => {
                    arguments.push("-change".to_owned());
                    arguments.push(change.old.clone());
                    arguments.push(change.new.clone());
                }
            }
        }
        arguments
    }
}

/// Call the mapper for every path-bearing command and build the edit plan.
pub(super) fn plan(
    parsed: &Parsed,
    map: &mut impl FnMut(PathKind, &str, &[&str]) -> Result<Option<String>, RelocateError>,
) -> Result<Plan, MachoError> {
    let rpaths: Vec<&str> = parsed
        .commands
        .iter()
        .filter(|command| command.kind == Some(Kind::Rpath))
        .filter_map(|command| command.name.as_deref())
        .collect();
    let mut mapped: HashMap<(PathKind, String), Option<String>> = HashMap::new();
    let mut expected = vec![None; parsed.commands.len()];
    let mut changes = Vec::new();

    for (index, command) in parsed.commands.iter().enumerate() {
        let Some(kind) = command.kind else {
            continue;
        };
        let old = command
            .name
            .as_deref()
            .ok_or(MachoError::CommandMissingPath { index })?;
        let path_kind = kind.path_kind();
        let mapped_value = map(path_kind, old, &rpaths).map_err(|source| MachoError::MapPath {
            label: kind.label(),
            old: old.to_owned(),
            source: Box::new(source),
        })?;
        if let Some(new) = &mapped_value {
            validate_name(new, kind, true)?;
        }
        let key = (path_kind, old.to_owned());
        if let Some(previous) = mapped.get(&key) {
            if previous != &mapped_value {
                return Err(MachoError::InconsistentMapper {
                    old: old.to_owned(),
                    previous: previous.clone(),
                    mapped: mapped_value,
                });
            }
        } else {
            mapped.insert(key, mapped_value.clone());
        }
        let target = mapped_value.unwrap_or_else(|| old.to_owned());
        if target.contains(MARKER_TEXT) {
            return Err(MachoError::UnresolvedPlaceholder {
                label: kind.label(),
                target,
            });
        }
        if target != old {
            changes.push(Change {
                kind,
                old: old.to_owned(),
                new: target.clone(),
            });
        }
        expected[index] = Some(target);
    }

    Ok(Plan { changes, expected })
}

/// Reject plans that collapse distinct commands onto one path.
pub(super) fn validate_targets(
    parsed: &Parsed,
    expected: &[Option<String>],
) -> Result<(), MachoError> {
    let mut dylibs: HashMap<&str, usize> = HashMap::new();
    let mut rpaths: HashMap<&str, usize> = HashMap::new();
    for (index, command) in parsed.commands.iter().enumerate() {
        let Some(kind) = command.kind else {
            continue;
        };
        let target = expected
            .get(index)
            .and_then(Option::as_deref)
            .ok_or(MachoError::NoTargetPlanned { index })?;
        let table = if kind.is_rpath() {
            &mut rpaths
        } else if kind == Kind::IdDylib {
            continue;
        } else {
            &mut dylibs
        };
        if let Some(previous) = table.insert(target, index) {
            return Err(MachoError::SharedPath {
                previous,
                index,
                target: target.to_owned(),
            });
        }
    }
    Ok(())
}

/// Require every raw marker to sit in a supported command path and be resolved
/// by the plan. This runs before anything is mutated.
pub(super) fn check_marker_plan(
    bytes: &[u8],
    parsed: &Parsed,
    expected: &[Option<String>],
) -> Result<(), MachoError> {
    let mut ranges: Vec<Range<usize>> = parsed
        .commands
        .iter()
        .filter_map(|command| command.name_range.clone())
        .collect();
    ranges.sort_unstable_by_key(|range| range.start);

    let mut range_index = 0usize;
    scan_markers(bytes, |offset| {
        while range_index < ranges.len() && ranges[range_index].end <= offset {
            range_index += 1;
        }
        let contained = range_index < ranges.len()
            && ranges[range_index].start <= offset
            && offset + MARKER.len() <= ranges[range_index].end;
        if !contained {
            return Err(MachoError::PlaceholderOutsideCommand { offset });
        }
        Ok(())
    })?;

    for (index, target) in expected.iter().enumerate() {
        if let Some(target) = target
            && target.contains(MARKER_TEXT)
        {
            return Err(MachoError::PlaceholderNotResolved {
                index,
                target: target.clone(),
            });
        }
    }
    Ok(())
}

/// Fail if any raw marker remains anywhere in `bytes`.
pub(super) fn reject_any_marker(bytes: &[u8]) -> Result<(), MachoError> {
    scan_markers(bytes, |offset| {
        Err(MachoError::PlaceholderRemains { offset })
    })
}

/// Visit the offset of every non-overlapping-width occurrence of [`MARKER`],
/// scanning the image once.
pub(super) fn scan_markers(
    bytes: &[u8],
    mut visit: impl FnMut(usize) -> Result<(), MachoError>,
) -> Result<(), MachoError> {
    let mut search_from = 0usize;
    while let Some(tail) = bytes.get(search_from..) {
        let Some(relative) = tail
            .windows(MARKER.len())
            .position(|window| window == MARKER)
        else {
            break;
        };
        let offset = search_from + relative;
        visit(offset)?;
        let Some(next) = offset.checked_add(1) else {
            break;
        };
        search_from = next;
    }
    Ok(())
}
