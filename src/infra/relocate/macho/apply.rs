//! Applying the plan with Apple tools, verifying the result and ad-hoc signing.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::constants::LC_CODE_SIGNATURE;
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
pub(super) fn apply(path: &Path, original: &Parsed, plan: &Plan) -> Result<()> {
    let arguments = plan.arguments();
    run_tool(
        INSTALL_NAME_TOOL,
        &arguments,
        path,
        "install_name_tool could not rewrite the staged image",
    )?;

    let rewritten = fs::read(path)
        .with_context(|| format!("Could not read rewritten image {}", path.display()))?;
    verify_stage(original, plan, &rewritten)
        .context("The image did not match the relocation plan after install_name_tool")?;

    let sign_arguments = signing_arguments(original.signed);
    run_tool(
        CODESIGN,
        &sign_arguments,
        path,
        "codesign could not ad-hoc sign the staged image",
    )?;

    let signed = fs::read(path)
        .with_context(|| format!("Could not read signed image {}", path.display()))?;
    verify_stage(original, plan, &signed)
        .context("The image did not match the relocation plan after codesign")?;

    let verify_arguments = ["--verify".to_owned(), "--strict".to_owned()];
    run_tool(
        CODESIGN,
        &verify_arguments,
        path,
        "codesign could not verify the staged image",
    )?;

    let verified = fs::read(path)
        .with_context(|| format!("Could not read verified image {}", path.display()))?;
    verify_stage(original, plan, &verified)
        .context("The image did not match the relocation plan after signature verification")?;
    Ok(())
}

/// Parse the rewritten bytes and check them against the original identity and
/// the exact plan.
pub(super) fn verify_stage(original: &Parsed, plan: &Plan, bytes: &[u8]) -> Result<()> {
    reject_any_marker(bytes)?;
    let parsed =
        parse(bytes).context("Rewritten file is not a supported thin 64-bit Mach-O image")?;
    validate_relocatable(&parsed).context("Rewritten load-command set is not relocatable")?;
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
) -> Result<()> {
    if after.header.cputype != original.header.cputype
        || after.header.cpusubtype != original.header.cpusubtype
        || after.header.filetype != original.header.filetype
    {
        bail!("Rewritten Mach-O header identity changed");
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
            bail!("Load command {index} disappeared from the rewritten image");
        };
        if actual.command != command.command {
            bail!(
                "Load command {index} changed type from {:#x} to {:#x}",
                command.command,
                actual.command
            );
        }
        match (&expected[index], actual.name.as_deref()) {
            (Some(target), Some(value)) if target == value => {}
            (Some(target), Some(value)) => {
                bail!(
                    "Load command {index} expected path {target:?}, but the rewritten image contains {value:?}"
                );
            }
            (Some(_), None) => {
                bail!("Load command {index} lost its path in the rewritten image");
            }
            (None, Some(value)) => {
                bail!(
                    "Load command {index} unexpectedly gained the path {value:?} in the rewritten image"
                );
            }
            (None, None) => {}
        }
    }
    if let Some(extra) = rewritten.next() {
        bail!(
            "Rewritten image gained an unexpected load command {:#x}",
            extra.command
        );
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
fn run_tool(program: &str, arguments: &[String], path: &Path, context: &str) -> Result<()> {
    let output = Command::new(program)
        .args(arguments)
        .arg(path)
        .output()
        .with_context(|| format!("Could not run {program}"))?;
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
    bail!("{context} ({}): {detail}", output.status)
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
pub(super) fn ensure_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect staged image {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("Staged image {} is not a regular file", path.display());
    }
    Ok(())
}
