//! The actions of the plugin, and the popups that they open.
//!
//! An action runs in the background, with the action context of herdr.
//! It opens a popup pane of the plugin, and gives it the workspace and
//! the folder of the context in 2 environment variables.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::git::{self, Checkout};
use crate::herdr::{self, Error};
use crate::wt;

const WORKSPACE_VARIABLE: &str = "TRUNK_WORKSPACE_ID";
const FOLDER_VARIABLE: &str = "TRUNK_FOLDER";

/// The action context that herdr gives to an action.
fn action_context() -> Result<serde_json::Value, Error> {
    let text = env::var("HERDR_PLUGIN_CONTEXT_JSON").map_err(|_| {
        Error::Command("No herdr action context: run this action from herdr.".to_owned())
    })?;
    serde_json::from_str(&text)
        .map_err(|error| Error::Command(format!("Invalid herdr action context: {error}")))
}

/// Action: open the popup `entrypoint` for the focused workspace.
pub fn open(entrypoint: &str) -> Result<(), Error> {
    let context = action_context()?;
    let workspace_id = context["workspace_id"].as_str().unwrap_or_default();
    let folder = context["focused_pane_cwd"]
        .as_str()
        .or_else(|| context["workspace_cwd"].as_str())
        .ok_or_else(|| Error::Command("The herdr action context has no folder.".to_owned()))?;
    herdr::open_plugin_pane(
        entrypoint,
        Path::new(folder),
        &[
            (WORKSPACE_VARIABLE, workspace_id),
            (FOLDER_VARIABLE, folder),
        ],
    )
}

/// Popup picker: pick or create a worktree with wt, then open its workspace.
pub fn pick() -> Result<(), Error> {
    let folder = match env::var_os(FOLDER_VARIABLE) {
        Some(folder) => PathBuf::from(folder),
        None => env::current_dir().map_err(|error| Error::Command(error.to_string()))?,
    };
    let repository = git::checkout_of(&folder).ok_or_else(|| {
        Error::Command(format!("{} is not in a git repository.", folder.display()))
    })?;
    let Some(picked) = wt::pick(&repository.root)? else {
        return Ok(());
    };
    let checkout = git::checkout_of(&picked)
        .ok_or_else(|| Error::Command(format!("{} is not a git checkout.", picked.display())))?;
    herdr::open_checkout(&checkout.root, &checkout.main_checkout(), true)?;
    Ok(())
}

/// The worktree of the workspace of the action, if no agent works in it.
fn target_worktree() -> Result<(String, Checkout), Error> {
    let workspace_id = env::var(WORKSPACE_VARIABLE).map_err(|_| {
        Error::Command("No workspace: run this popup from its herdr action.".to_owned())
    })?;
    let worktree = herdr::workspace_checkout(&workspace_id)?
        .filter(|checkout| checkout.is_linked_worktree)
        .ok_or_else(|| {
            Error::Command("This workspace does not show a linked worktree.".to_owned())
        })?;
    if herdr::workspace_has_agent(&workspace_id)? {
        return Err(Error::Command(
            "An agent works in this workspace. Stop the agent first: the removal deletes its folder."
                .to_owned(),
        ));
    }
    let checkout = git::checkout_of(&worktree.checkout_path).ok_or_else(|| {
        Error::Command(format!(
            "{} is not a git checkout.",
            worktree.checkout_path.display()
        ))
    })?;
    Ok((workspace_id, checkout))
}

/// Ask a yes or no question on the terminal. The default is no.
fn confirm(question: &str) -> Result<bool, Error> {
    print!("{question} [y/N] ");
    io::stdout()
        .flush()
        .map_err(|error| Error::Command(error.to_string()))?;
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| Error::Command(error.to_string()))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// After a removal: give the focus to the main checkout, then close the
/// workspace of the removed worktree.
fn close_after_removal(workspace_id: &str, checkout: &Checkout) -> Result<(), Error> {
    let main_checkout = checkout.main_checkout();
    herdr::open_checkout(&main_checkout, &main_checkout, true)?;
    match herdr::close_workspace(workspace_id) {
        // The observer can close the workspace first.
        Err(Error::Herdr { code, .. }) if code == "workspace_not_found" => Ok(()),
        other => other,
    }
}

/// Popup remover: remove the worktree of the workspace, after a question.
pub fn remove() -> Result<(), Error> {
    let (workspace_id, checkout) = target_worktree()?;
    println!("{}", checkout.root.display());
    let question = "Remove this worktree? wt keeps a branch that is not merged.";
    if !confirm(question)? {
        return Ok(());
    }
    wt::remove(&checkout.main_checkout(), &checkout.root)?;
    close_after_removal(&workspace_id, &checkout)
}

/// Popup merger: merge the worktree of the workspace into the default
/// branch with wt, then remove it, after a question.
pub fn merge() -> Result<(), Error> {
    let (workspace_id, checkout) = target_worktree()?;
    println!("{}", checkout.root.display());
    let question = "Merge this worktree into the default branch (wt merge), then remove it?";
    if !confirm(question)? {
        return Ok(());
    }
    wt::merge(&checkout.root)?;
    wt::remove(&checkout.main_checkout(), &checkout.root)?;
    close_after_removal(&workspace_id, &checkout)
}
