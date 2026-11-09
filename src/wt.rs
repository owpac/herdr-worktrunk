//! The calls to worktrunk (wt). wt creates and removes the worktrees, and
//! runs their hooks.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::herdr::Error;

/// Open the worktree of `branch` with wt (wt creates it if necessary),
/// and return its path. With `is_new_branch`, wt also creates the branch
/// from the default branch. The wt hooks run, and their questions use the
/// terminal of the user.
pub fn switch(repository: &Path, branch: &str, is_new_branch: bool) -> Result<PathBuf, Error> {
    let mut command = Command::new("wt");
    command.arg("-C").arg(repository).arg("switch");
    if is_new_branch {
        command.arg("--create");
    }
    let output = command
        .args([branch, "--no-cd", "--format", "json"])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| Error::Command(format!("cannot run wt: {error}")))?;
    if !output.status.success() {
        return Err(Error::Command(format!(
            "wt did not open the worktree of {branch}."
        )));
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::Command(format!("wt switch: invalid JSON: {error}")))?;
    let path = result["path"]
        .as_str()
        .ok_or_else(|| Error::Command("wt switch gave no path.".to_owned()))?;
    Ok(PathBuf::from(path))
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

/// Remove the worktree `checkout` without output on the terminal. On a
/// failure, the error has the message of wt.
pub fn remove_quietly(main_checkout: &Path, checkout: &Path) -> Result<(), Error> {
    remove_without_terminal(main_checkout, checkout, &[])
}

/// Remove the worktree `checkout` of a finished pull request, and its
/// branch, without output on the terminal.
///
/// The pull request contains the local commit of the branch, so GitHub
/// keeps its work: in the default branch after a merge, in
/// `refs/pull/<number>/head` after a close. git does not see the work in
/// the default branch after a squash merge or a close: `--force-delete`
/// deletes the branch in these cases too. wt refuses a worktree with
/// changes that are not committed.
pub fn remove_finished(main_checkout: &Path, checkout: &Path) -> Result<(), Error> {
    remove_without_terminal(main_checkout, checkout, &["--force-delete"])
}

/// Remove the worktree `checkout` with wt and `options`. On a failure, the
/// error has the message of wt.
fn remove_without_terminal(
    main_checkout: &Path,
    checkout: &Path,
    options: &[&str],
) -> Result<(), Error> {
    let output = Command::new("wt")
        .arg("-C")
        .arg(main_checkout)
        .args(["remove", "--foreground"])
        .args(options)
        .arg(checkout)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::Command(format!("cannot run wt: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let details = String::from_utf8_lossy(&output.stderr);
    Err(Error::Command(format!(
        "wt did not delete the worktree.\n{}",
        details.trim()
    )))
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
