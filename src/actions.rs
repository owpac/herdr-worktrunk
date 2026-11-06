//! The actions of the plugin, and the popups that they open.
//!
//! An action runs in the background, with the action context of herdr.
//! It first checks that its popup can work: a git repository for the
//! picker, a linked worktree without agent for the remover and the
//! merger. If the check fails, it opens the small popup `message`. Else
//! it opens its popup, and gives it the workspace and the folder of the
//! context in environment variables.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::git::{self, Checkout};
use crate::herdr::{self, Error};
use crate::ui;
use crate::wt;

const WORKSPACE_VARIABLE: &str = "TRUNK_WORKSPACE_ID";
const FOLDER_VARIABLE: &str = "TRUNK_FOLDER";
const TITLE_VARIABLE: &str = "TRUNK_MESSAGE_TITLE";
const MESSAGE_VARIABLE: &str = "TRUNK_MESSAGE";
const MESSAGE_ENTRYPOINT: &str = "message";

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
    let folder_path = Path::new(folder);
    let check = match entrypoint {
        "picker" => repository_of(folder_path).map(|_| ()),
        _ => target_worktree(workspace_id).map(|_| ()),
    };
    if let Err(error) = check {
        return herdr::open_plugin_pane(
            MESSAGE_ENTRYPOINT,
            folder_path,
            &[
                (TITLE_VARIABLE, popup_title(entrypoint)),
                (MESSAGE_VARIABLE, &error.to_string()),
            ],
        );
    }
    herdr::open_plugin_pane(
        entrypoint,
        folder_path,
        &[
            (WORKSPACE_VARIABLE, workspace_id),
            (FOLDER_VARIABLE, folder),
        ],
    )
}

/// The title of the message box of a popup.
pub fn popup_title(entrypoint: &str) -> &'static str {
    match entrypoint {
        "picker" | "pick" => "Worktrunk",
        "remover" | "remove" => "Worktrunk: remove",
        _ => "Worktrunk: merge",
    }
}

/// Popup message: show the message of the action, with an OK button.
pub fn message() {
    let title = env::var(TITLE_VARIABLE).unwrap_or_else(|_| "Worktrunk".to_owned());
    let message = env::var(MESSAGE_VARIABLE).unwrap_or_default();
    ui::show_message(&title, &message);
}

/// The checkout that holds `folder`, or an error for the user.
fn repository_of(folder: &Path) -> Result<Checkout, Error> {
    git::checkout_of(folder).ok_or_else(|| {
        Error::Command(format!(
            "This folder is not in a git repository:\n{}",
            folder.display()
        ))
    })
}

/// Popup picker: pick or create a worktree with wt, then open its workspace.
pub fn pick() -> Result<(), Error> {
    let folder = match env::var_os(FOLDER_VARIABLE) {
        Some(folder) => PathBuf::from(folder),
        None => env::current_dir().map_err(|error| Error::Command(error.to_string()))?,
    };
    let repository = repository_of(&folder)?;
    let Some(picked) = wt::pick(&repository.root)? else {
        return Ok(());
    };
    let checkout = git::checkout_of(&picked)
        .ok_or_else(|| Error::Command(format!("{} is not a git checkout.", picked.display())))?;
    herdr::open_checkout(&checkout.root, &checkout.main_checkout(), true)?;
    Ok(())
}

/// The workspace of the popup, from its action.
fn popup_workspace() -> Result<String, Error> {
    env::var(WORKSPACE_VARIABLE).map_err(|_| {
        Error::Command("No workspace: run this popup from its herdr action.".to_owned())
    })
}

/// The worktree of a workspace, if no agent works in it.
fn target_worktree(workspace_id: &str) -> Result<Checkout, Error> {
    let worktree = herdr::workspace_checkout(workspace_id)?
        .filter(|checkout| checkout.is_linked_worktree)
        .ok_or_else(|| {
            Error::Command(
                "This workspace does not show a linked worktree. Open the workspace of a worktree first."
                    .to_owned(),
            )
        })?;
    if herdr::workspace_has_agent(workspace_id)? {
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
    Ok(checkout)
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
    let workspace_id = popup_workspace()?;
    let checkout = target_worktree(&workspace_id)?;
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
    let workspace_id = popup_workspace()?;
    let checkout = target_worktree(&workspace_id)?;
    println!("{}", checkout.root.display());
    let question = "Merge this worktree into the default branch (wt merge), then remove it?";
    if !confirm(question)? {
        return Ok(());
    }
    wt::merge(&checkout.root)?;
    wt::remove(&checkout.main_checkout(), &checkout.root)?;
    close_after_removal(&workspace_id, &checkout)
}
