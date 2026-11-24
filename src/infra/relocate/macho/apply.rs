//! Applying the plan with Apple tools, verifying the result and ad-hoc signing.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::constants::LC_CODE_SIGNATURE;
use super::error::MachoError;
use super::model::Parsed;
use super::parse::{parse, validate_relocatable};
use super::rewrite_plan::{Plan, reject_any_marker};

/// Maximum tool diagnostic copied into an error.
const MAX_TOOL_DIAGNOSTIC_BYTES: usize = 4096;

/// Absolute path of Apple's install-name rewriter.
const INSTALL_NAME_TOOL: &str = "/usr/bin/install_name_tool";
/// Absolute path of Apple's code-signing tool.
const CODESIGN: &str = "/usr/bin/codesign";

/// Run the Apple rewriter, sign, and verify, re-parsing after every mutation.
pub(super) fn apply(path: &Path, original: &Parsed, plan: &Plan) -> Result<(), MachoError> {
    let arguments = plan.arguments();
    run_tool(
        INSTALL_NAME_TOOL,
        &arguments,
        path,
        "install_name_tool could not rewrite the staged image",
    )?;

    let rewritten = read_image(path, "rewritten")?;
    verify_stage(original, plan, &rewritten).map_err(plan_mismatch("install_name_tool"))?;

    let sign_arguments = signing_arguments(original.signed);
    run_tool(
        CODESIGN,
        &sign_arguments,
        path,
        "codesign could not ad-hoc sign the staged image",
    )?;

    let signed = read_image(path, "signed")?;
    verify_stage(original, plan, &signed).map_err(plan_mismatch("codesign"))?;

    let verify_arguments = ["--verify".to_owned(), "--strict".to_owned()];
    run_tool(
        CODESIGN,
        &verify_arguments,
        path,
        "codesign could not verify the staged image",
    )?;

    let verified = read_image(path, "verified")?;
    verify_stage(original, plan, &verified).map_err(plan_mismatch("signature verification"))?;
    Ok(())
}

/// Read back an image the tools just wrote.
fn read_image(path: &Path, stage: &'static str) -> Result<Vec<u8>, MachoError> {
    fs::read(path).map_err(|source| MachoError::ReadImage {
        stage,
        path: path.to_path_buf(),
        source,
    })
}

/// Wrap a verification failure with the tool step that preceded it.
fn plan_mismatch(stage: &'static str) -> impl FnOnce(MachoError) -> MachoError {
    move |source| MachoError::PlanMismatch {
        stage,
        source: Box::new(source),
    }
}

/// Parse the rewritten bytes and check them against the original identity and
/// the exact plan.
pub(super) fn verify_stage(original: &Parsed, plan: &Plan, bytes: &[u8]) -> Result<(), MachoError> {
    reject_any_marker(bytes)?;
    let parsed = parse(bytes).map_err(|source| MachoError::RewrittenUnsupported {
        source: Box::new(source),
    })?;
    validate_relocatable(&parsed).map_err(|source| MachoError::RewrittenNotRelocatable {
        source: Box::new(source),
    })?;
    compare_rewrite(original, &plan.expected, &parsed)
}

/// Compare the non-signature load commands of a rewritten image with the plan.
///
/// `codesign` may add, replace, or remove `LC_CODE_SIGNATURE`; every other
/// command must keep its type, position relative to other non-signature
/// commands, and (for path-bearing commands) its planned value. No command may
/// appear that the original image did not have.
pub(super) fn compare_rewrite(
    original: &Parsed,
    expected: &[Option<String>],
    after: &Parsed,
) -> Result<(), MachoError> {
    if after.header.cputype != original.header.cputype
        || after.header.cpusubtype != original.header.cpusubtype
        || after.header.filetype != original.header.filetype
    {
        return Err(MachoError::HeaderIdentityChanged);
    }

    let mut rewritten = after
        .commands
        .iter()
        .filter(|command| command.command != LC_CODE_SIGNATURE);
    for (index, command) in original.commands.iter().enumerate() {
        if command.command == LC_CODE_SIGNATURE {
            continue;
        }
        let Some(actual) = rewritten.next() else {
            return Err(MachoError::CommandDisappeared { index });
        };
        if actual.command != command.command {
            return Err(MachoError::CommandTypeChanged {
                index,
                from: command.command,
                to: actual.command,
            });
        }
        match (&expected[index], actual.name.as_deref()) {
            (Some(target), Some(value)) if target == value => {}
            (Some(target), Some(value)) => {
                return Err(MachoError::PathMismatch {
                    index,
                    expected: target.clone(),
                    actual: value.to_owned(),
                });
            }
            (Some(_), None) => {
                return Err(MachoError::PathLost { index });
            }
            (None, Some(value)) => {
                return Err(MachoError::PathGained {
                    index,
                    value: value.to_owned(),
                });
            }
            (None, None) => {}
        }
    }
    if let Some(extra) = rewritten.next() {
        return Err(MachoError::ExtraCommand {
            command: extra.command,
        });
    }
    Ok(())
}

/// Build the ad-hoc signing arguments for a rebuilt image.
///
/// Only an image that arrived with a real signature can preserve metadata; an
/// unsigned image has nothing to preserve and is signed plainly.
pub(super) fn signing_arguments(originally_signed: bool) -> Vec<String> {
    let mut arguments = vec!["--force".to_owned(), "--sign".to_owned(), "-".to_owned()];
    if originally_signed {
        arguments.push("--preserve-metadata=entitlements,requirements,flags,runtime".to_owned());
    }
    arguments
}

/// Run one absolute Apple tool with `arguments` followed by `path`.
fn run_tool(
    program: &'static str,
    arguments: &[String],
    path: &Path,
    context: &'static str,
) -> Result<(), MachoError> {
    let output = Command::new(program)
        .args(arguments)
        .arg(path)
        .output()
        .map_err(|source| MachoError::ToolSpawn { program, source })?;
    if output.status.success() {
        return Ok(());
    }
    let mut diagnostics: Vec<String> = Vec::new();
    if let Some(stdout) = diagnostic(&output.stdout) {
        diagnostics.push(format!("stdout: {stdout}"));
    }
    if let Some(stderr) = diagnostic(&output.stderr) {
        diagnostics.push(format!("stderr: {stderr}"));
    }
    let detail = if diagnostics.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        diagnostics.join("; ")
    };
    Err(MachoError::ToolFailed {
        context,
        status: output.status,
        detail,
    })
}

/// Render a bounded, lossy version of one tool stream.
fn diagnostic(bytes: &[u8]) -> Option<String> {
    let end = bytes.len().min(MAX_TOOL_DIAGNOSTIC_BYTES);
    let text = String::from_utf8_lossy(&bytes[..end]);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Require a regular, non-symlink staged file.
pub(super) fn ensure_regular_file(path: &Path) -> Result<(), MachoError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MachoError::InspectImage {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(MachoError::ImageNotRegular {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}
