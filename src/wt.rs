//! The calls to worktrunk (wt). wt creates and removes the worktrees, and
//! runs their hooks.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::herdr::Error;

/// Show the picker of wt, and return the checkout that the user picks.
///
/// The picker lists the worktrees and the local branches. Enter opens a
/// row (wt creates the worktree of a branch), alt-c creates a worktree
/// with the typed name, alt-x removes a row, and Esc cancels. The picker
/// draws on the terminal, and prints its JSON result on stdout.
///
/// Returns None if the user cancels.
pub fn pick(repository: &Path) -> Result<Option<PathBuf>, Error> {
    let output = Command::new("wt")
        .arg("-C")
        .arg(repository)
        .args(["switch", "--branches", "--no-cd", "--format", "json"])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| Error::Command(format!("cannot run wt: {error}")))?;
    if !output.status.success() {
        return Err(Error::Command("The wt picker failed.".to_owned()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Ok(None);
    }
    let result: Value = serde_json::from_str(text.trim())
        .map_err(|error| Error::Command(format!("wt switch: invalid JSON: {error}")))?;
    let path = result["path"]
        .as_str()
        .ok_or_else(|| Error::Command("wt switch gave no path.".to_owned()))?;
    Ok(Some(PathBuf::from(path)))
}

/// Run a wt command on the terminal of the user, and fail if it fails.
fn run_on_terminal(arguments: &[&str], folder: &Path, failure: &str) -> Result<(), Error> {
    let status = Command::new("wt")
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .status()
        .map_err(|error| Error::Command(format!("cannot run wt: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Command(failure.to_owned()))
    }
}

/// Remove the worktree `checkout`. wt runs from the main checkout, so it
/// has no folder to leave. wt refuses a worktree with changes that are
/// not committed, and keeps a branch that is not merged.
pub fn remove(main_checkout: &Path, checkout: &Path) -> Result<(), Error> {
    let checkout_text = checkout.to_string_lossy();
    run_on_terminal(
        &["remove", "--foreground", &checkout_text],
        main_checkout,
        "wt did not remove the worktree (see above).",
    )
}

/// Merge the branch of `checkout` into the default branch, without the
/// removal of the worktree. wt squashes, rebases, runs the pre-merge
/// hooks and fast-forwards the default branch.
///
/// wt removes a worktree in the background after a merge. Thus this
/// function asks wt to keep it: `remove` removes it in the foreground,
/// before the plugin closes its workspace.
pub fn merge(checkout: &Path) -> Result<(), Error> {
    run_on_terminal(
        &["merge", "--no-remove"],
        checkout,
        "wt did not merge the worktree (see above).",
    )
}
