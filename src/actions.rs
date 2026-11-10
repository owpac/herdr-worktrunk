//! The actions of the plugin, and the popups that they open.
//!
//! The popups:
//!
//! - `picker` (large): the fzf picker of the worktrees.
//! - `confirm` (small): the question before a deletion or a merge.
//! - `merger` (large): the output of `wt merge`.
//! - `message` (small): a message with an OK button.
//!
//! An action runs in the background, with the action context of herdr.
//! It first checks that its popup can work: a git repository for the
//! picker, a linked worktree without agent for a deletion or a merge. If
//! the check fails, it opens the popup `message`.
//!
//! herdr shows one popup at a time. Thus a popup that needs another popup
//! (the picker that asks to delete a worktree) starts a detached process,
//! then closes. The detached process opens the next popup when herdr is
//! ready (`open-later`). After the question, the picker comes back.

use std::env;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::cleaner;
use crate::git::{self, Checkout};
use crate::herdr::{self, Error};
use crate::leftover::{self, Agents, WorktreeAgent};
use crate::picker::{self, Picked, Row};
use crate::ui;
use crate::wt;

const WORKSPACE_VARIABLE: &str = "TRUNK_WORKSPACE_ID";
const FOLDER_VARIABLE: &str = "TRUNK_FOLDER";
const CHECKOUT_VARIABLE: &str = "TRUNK_CHECKOUT";
const OPERATION_VARIABLE: &str = "TRUNK_OPERATION";
const RETURN_VARIABLE: &str = "TRUNK_RETURN";
const REPOSITORY_VARIABLE: &str = "TRUNK_REPOSITORY";
const TITLE_VARIABLE: &str = "TRUNK_MESSAGE_TITLE";
const MESSAGE_VARIABLE: &str = "TRUNK_MESSAGE";
/// The reason of a question that the observer asks (an old worktree).
const REASON_VARIABLE: &str = "TRUNK_REASON";
/// A deletion of the popup cleaner: the idle agents of the worktree close
/// first. The cleaner gives it only to a worktree with a proof of the end.
const CLOSE_IDLE_VARIABLE: &str = "TRUNK_CLOSE_IDLE_AGENTS";
/// The state file of the popup cleaner: the results of its checks. The
/// cleaner comes back after a question with these results.
const STATE_VARIABLE: &str = "TRUNK_CLEANER_STATE";
/// The repository of the popup cleaner, else all the repositories of the
/// herdr workspaces.
const SCOPE_VARIABLE: &str = "TRUNK_CLEANER_SCOPE";
/// `TRUNK_RETURN=cleaner`: after the question, the cleaner comes back.
const RETURN_TO_CLEANER: &str = "cleaner";
/// The popup confirm asks before the deletion of all the old worktrees.
const DELETE_ALL: &str = "delete-all";

/// The observer waits this number of times for a free popup, 2 seconds
/// each time: 10 minutes at most.
const ASK_ATTEMPTS: u32 = 300;
const ASK_INTERVAL: Duration = Duration::from_secs(2);

/// The popup `confirm` asks before one of these operations.
const DELETE: &str = "delete";
const MERGE: &str = "merge";
/// `TRUNK_RETURN=picker`: after the question, the picker comes back.
const RETURN_TO_PICKER: &str = "picker";

/// herdr refuses a popup while another popup is open: the detached
/// process tries again for 5 seconds.
const OPEN_LATER_ATTEMPTS: u32 = 50;
const OPEN_LATER_INTERVAL: Duration = Duration::from_millis(100);

const TITLE: &str = "Worktrunk";

/// The action context that herdr gives to an action.
fn action_context() -> Result<serde_json::Value, Error> {
    let text = env::var("HERDR_PLUGIN_CONTEXT_JSON").map_err(|_| {
        Error::Command("No herdr action context: run this action from herdr.".to_owned())
    })?;
    serde_json::from_str(&text)
        .map_err(|error| Error::Command(format!("Invalid herdr action context: {error}")))
}

/// The title of the box of a popup.
pub fn popup_title(command: &str) -> &'static str {
    match command {
        "merger" => "Worktrunk: merge",
        _ => TITLE,
    }
}

/// Action: open the popup of `entrypoint` (picker, remover or merger) for
/// the focused workspace, or a message if the popup cannot work.
pub fn open(entrypoint: &str) -> Result<(), Error> {
    let context = action_context()?;
    let workspace_id = context["workspace_id"].as_str().unwrap_or_default();
    let folder = context["focused_pane_cwd"]
        .as_str()
        .or_else(|| context["workspace_cwd"].as_str())
        .ok_or_else(|| Error::Command("The herdr action context has no folder.".to_owned()))?;
    let folder_path = Path::new(folder);
    if entrypoint == "cleaner" {
        return herdr::open_plugin_pane("cleaner", folder_path, &[(FOLDER_VARIABLE, folder)]);
    }
    if entrypoint == "picker" {
        if let Err(error) = repository_of(folder_path) {
            return open_message(&error.to_string(), folder_path);
        }
        return herdr::open_plugin_pane(
            "picker",
            folder_path,
            &[
                (WORKSPACE_VARIABLE, workspace_id),
                (FOLDER_VARIABLE, folder),
            ],
        );
    }
    let checkout = match target_worktree(workspace_id) {
        Ok(checkout) => checkout,
        Err(error) => return open_message(&error.to_string(), folder_path),
    };
    let operation = if entrypoint == "merger" {
        MERGE
    } else {
        DELETE
    };
    herdr::open_plugin_pane(
        "confirm",
        folder_path,
        &[
            (OPERATION_VARIABLE, operation),
            (CHECKOUT_VARIABLE, &checkout.root.to_string_lossy()),
            (WORKSPACE_VARIABLE, workspace_id),
        ],
    )
}

fn open_message(message: &str, folder: &Path) -> Result<(), Error> {
    herdr::open_plugin_pane(
        "message",
        folder,
        &[(TITLE_VARIABLE, TITLE), (MESSAGE_VARIABLE, message)],
    )
}

/// Popup message: show the message of the action, with an OK button.
pub fn message() {
    let title = env::var(TITLE_VARIABLE).unwrap_or_else(|_| TITLE.to_owned());
    let message = env::var(MESSAGE_VARIABLE).unwrap_or_default();
    ui::show_message(&title, &message);
}

/// Start a detached process that opens the popup `entrypoint` when this
/// popup is closed. The caller must close its popup (exit) after this.
fn open_after_this_popup(
    entrypoint: &str,
    folder: &Path,
    environment: &[(&str, &str)],
) -> Result<(), Error> {
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let assignments: Vec<String> = environment
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    Command::new(executable)
        .arg("open-later")
        .arg(entrypoint)
        .arg(folder)
        .args(&assignments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| Error::Command(format!("cannot open the next popup: {error}")))?;
    Ok(())
}

/// Detached process `open-later`: open the popup `entrypoint` as soon as
/// herdr accepts it. Each assignment has the form `NAME=VALUE`.
pub fn open_later(entrypoint: &str, folder: &Path, assignments: &[&str]) {
    let environment: Vec<(&str, &str)> = assignments
        .iter()
        .filter_map(|assignment| assignment.split_once('='))
        .collect();
    for _ in 0..OPEN_LATER_ATTEMPTS {
        match herdr::open_plugin_pane(entrypoint, folder, &environment) {
            Err(Error::Herdr { code, .. }) if code == "ui_busy" => {
                thread::sleep(OPEN_LATER_INTERVAL);
            }
            // Success, or an error that a new try cannot correct.
            _ => return,
        }
    }
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

/// The folder of a popup, from its action.
fn popup_folder() -> Result<PathBuf, Error> {
    match env::var_os(FOLDER_VARIABLE) {
        Some(folder) => Ok(PathBuf::from(folder)),
        None => env::current_dir().map_err(|error| Error::Command(error.to_string())),
    }
}

/// Popup picker: pick or create a worktree, then open its workspace. A
/// deletion goes to the popup confirm, then the picker comes back.
pub fn pick() -> Result<(), Error> {
    let folder = popup_folder()?;
    let repository = repository_of(&folder)?;
    let main_checkout = repository.main_checkout();
    let Some(picked) = picker::pick(&repository.root)? else {
        return Ok(());
    };
    let row = match picked {
        Picked::Open(row) => row,
        Picked::Delete(row) => {
            let Row::Worktree { path, .. } = row else {
                return Ok(());
            };
            let folder_text = folder.to_string_lossy();
            let repository_text = main_checkout.to_string_lossy();
            return open_after_this_popup(
                "confirm",
                &folder,
                &[
                    (OPERATION_VARIABLE, DELETE),
                    (CHECKOUT_VARIABLE, &path.to_string_lossy()),
                    (RETURN_VARIABLE, RETURN_TO_PICKER),
                    (FOLDER_VARIABLE, &folder_text),
                    (REPOSITORY_VARIABLE, &repository_text),
                ],
            );
        }
    };
    let picked_path = match row {
        Row::Worktree { path, .. } => path,
        Row::Branch { branch } => wt::switch(&main_checkout, &branch, false)?,
        Row::Create { name } => wt::switch(&main_checkout, &name, true)?,
    };
    let checkout = git::checkout_of(&picked_path).ok_or_else(|| {
        Error::Command(format!("{} is not a git checkout.", picked_path.display()))
    })?;
    herdr::open_checkout(&checkout.root, &checkout.main_checkout(), true)?;
    Ok(())
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
    refuse_if_agent(workspace_id)?;
    git::checkout_of(&worktree.checkout_path).ok_or_else(|| {
        Error::Command(format!(
            "{} is not a git checkout.",
            worktree.checkout_path.display()
        ))
    })
}

/// Refuse a deletion or a merge if an agent works in the workspace.
fn refuse_if_agent(workspace_id: &str) -> Result<(), Error> {
    if herdr::workspace_has_agent(workspace_id)? {
        return Err(Error::Command(
            "An agent works in the workspace of this worktree. Stop the agent first: the deletion removes its folder."
                .to_owned(),
        ));
    }
    Ok(())
}

/// The idle agents that a deletion of the cleaner closes first, or an
/// error if an agent works in the worktree now.
fn agents_to_close(
    checkout: &Path,
    workspace_id: Option<&str>,
) -> Result<Vec<WorktreeAgent>, Error> {
    let snapshot = herdr::snapshot()?;
    let agents = leftover::agents_of(&snapshot, checkout, workspace_id);
    match leftover::agents_state(&agents) {
        Agents::None => Ok(Vec::new()),
        Agents::Idle => Ok(agents),
        Agents::Busy => Err(Error::Command(
            "An agent works in this worktree. Stop it first: the deletion removes its folder."
                .to_owned(),
        )),
    }
}

/// Close the panes of idle agents, then wait until herdr shows no agent in
/// the worktree, for 10 seconds at most. The conversations stay on the
/// disk.
fn close_agents(agents: &[WorktreeAgent], checkout: &Path) -> Result<(), Error> {
    for agent in agents {
        herdr::close_pane(&agent.pane_id)?;
    }
    for _ in 0..20 {
        if !herdr::agent_in_checkout(checkout)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(Error::Command(
        "The idle agent of this worktree did not close.".to_owned(),
    ))
}

/// Popup confirm: ask before a deletion or a merge, then do it. If the
/// picker opened this popup, the picker comes back after.
pub fn confirm() -> Result<(), Error> {
    let result = ask_and_run();
    let return_to = env::var(RETURN_VARIABLE).unwrap_or_default();
    if return_to != RETURN_TO_PICKER && return_to != RETURN_TO_CLEANER {
        return result;
    }
    if let Err(error) = &result {
        ui::show_message(TITLE, &error.to_string());
    }
    if return_to == RETURN_TO_CLEANER {
        // The cleaner comes back with the results of its checks, without
        // the deleted worktrees.
        let folder = popup_folder()?;
        let folder = if folder.exists() {
            folder
        } else {
            env::temp_dir()
        };
        let state = env::var(STATE_VARIABLE).unwrap_or_default();
        let folder_text = folder.to_string_lossy();
        let mut environment = vec![
            (STATE_VARIABLE, state.as_str()),
            (FOLDER_VARIABLE, folder_text.as_ref()),
            (RETURN_VARIABLE, RETURN_TO_CLEANER),
        ];
        let scope = env::var(SCOPE_VARIABLE).unwrap_or_default();
        if !scope.is_empty() {
            environment.push((SCOPE_VARIABLE, scope.as_str()));
        }
        return open_after_this_popup("cleaner", &folder, &environment);
    }
    // The folder of the picker can be the deleted worktree: then the
    // picker comes back in the main checkout.
    let mut folder = popup_folder()?;
    if git::checkout_of(&folder).is_none() {
        if let Some(repository) = env::var_os(REPOSITORY_VARIABLE) {
            folder = PathBuf::from(repository);
        }
    }
    let folder_text = folder.to_string_lossy();
    open_after_this_popup("picker", &folder, &[(FOLDER_VARIABLE, &folder_text)])
}

/// The name of a worktree in a question: its branch, then its folder.
fn describe(checkout: &Checkout) -> (String, String) {
    let branch = git::branch_of(&checkout.root).unwrap_or_else(|| "this worktree".to_owned());
    let folder_name = checkout
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (branch, folder_name)
}

/// The question of the popup confirm, and its operation.
fn ask_and_run() -> Result<(), Error> {
    let operation = env::var(OPERATION_VARIABLE).unwrap_or_default();
    if operation == DELETE_ALL {
        return delete_all();
    }
    let checkout_path = env::var_os(CHECKOUT_VARIABLE)
        .map(PathBuf::from)
        .ok_or_else(|| Error::Command("No worktree: run this popup from its action.".to_owned()))?;
    let checkout = git::checkout_of(&checkout_path).ok_or_else(|| {
        Error::Command(format!(
            "{} is not a git checkout.",
            checkout_path.display()
        ))
    })?;
    let workspace_from_action = env::var(WORKSPACE_VARIABLE).ok();
    let workspace_id = match &workspace_from_action {
        Some(workspace_id) => Some(workspace_id.clone()),
        None => herdr::workspace_of_checkout(&checkout.root)?,
    };
    // A deletion of the cleaner closes the idle agents of a worktree with a
    // proof of the end. Any other agent stops the deletion.
    let close_idle = operation != MERGE && env::var_os(CLOSE_IDLE_VARIABLE).is_some();
    let idle_agents = if close_idle {
        agents_to_close(&checkout.root, workspace_id.as_deref())?
    } else {
        if let Some(workspace_id) = &workspace_id {
            refuse_if_agent(workspace_id)?;
        }
        if herdr::agent_in_checkout(&checkout.root)? {
            return Err(Error::Command(
                "An agent works in the folder of this worktree. Stop the agent first: the deletion removes its folder."
                    .to_owned(),
            ));
        }
        Vec::new()
    };
    let (branch, folder_name) = describe(&checkout);
    if operation == MERGE {
        let question = format!(
            "Merge {branch} into the default branch (wt merge), then delete its worktree?\n{folder_name}"
        );
        if !ui::confirm("Merge the worktree", &question, "Merge", false) {
            return Ok(());
        }
        let checkout_text = checkout.root.to_string_lossy();
        let mut environment = vec![(CHECKOUT_VARIABLE, checkout_text.as_ref())];
        if let Some(workspace_id) = &workspace_id {
            environment.push((WORKSPACE_VARIABLE, workspace_id.as_str()));
        }
        return open_after_this_popup("merger", &checkout.root, &environment);
    }
    let mut question = format!(
        "Delete the worktree of {branch}?\n{folder_name}\nwt keeps the branch if it is not merged."
    );
    if let Ok(reason) = env::var(REASON_VARIABLE) {
        question = format!("{reason}\n{question}");
    }
    if let Some(agent) = idle_agents.first() {
        question.push_str(&format!(
            "\nIts idle agent closes first. To resume it: {}",
            agent.resume_command()
        ));
    }
    if !ui::confirm("Delete the worktree", &question, "Delete", true) {
        return Ok(());
    }
    ui::show_progress(
        "Delete the worktree",
        &format!("Deleting the worktree of {branch}…"),
    );
    if !idle_agents.is_empty() {
        close_agents(&idle_agents, &checkout.root)?;
    }
    wt::remove_quietly(&checkout.main_checkout(), &checkout.root)?;
    if let Some(workspace_id) = &workspace_id {
        close_after_removal(workspace_id, &checkout)?;
    }
    Ok(())
}

/// A number of worktrees: `1 worktree`, `3 worktrees`.
fn worktrees_text(count: usize) -> String {
    if count == 1 {
        "1 worktree".to_owned()
    } else {
        format!("{count} worktrees")
    }
}

/// The question before the deletion of all the old worktrees of the list
/// of the cleaner, then the deletions and a summary.
fn delete_all() -> Result<(), Error> {
    // The state file keeps the results of the checks: the cleaner reads it
    // again after the question.
    let state_path = PathBuf::from(env::var_os(STATE_VARIABLE).unwrap_or_default());
    let old: Vec<cleaner::Entry> = cleaner::leftovers(&state_path)
        .into_iter()
        .filter(|entry| entry.path.exists())
        .collect();
    if old.is_empty() {
        return Ok(());
    }
    let shown: Vec<String> = old
        .iter()
        .take(5)
        .map(|worktree| format!("- {}", worktree.branch))
        .collect();
    let more = if old.len() > shown.len() {
        format!("\n… and {} more", old.len() - shown.len())
    } else {
        String::new()
    };
    let question = format!(
        "Delete {} of the group To delete?\n{}{more}\nwt keeps a branch that is not merged.",
        worktrees_text(old.len()),
        shown.join("\n")
    );
    if !ui::confirm("Clean the worktrees", &question, "Delete all", true) {
        return Ok(());
    }
    let mut deleted = 0;
    let mut kept = Vec::new();
    let mut closed = Vec::new();
    for worktree in &old {
        ui::show_progress(
            "Clean the worktrees",
            &format!("Deleting the worktree of {}…", worktree.branch),
        );
        // An error keeps this worktree, and the loop goes on: the summary
        // tells what happened to each worktree.
        let workspace_id = match herdr::workspace_of_checkout(&worktree.path) {
            Ok(workspace_id) => workspace_id,
            Err(error) => {
                kept.push(format!("{}: herdr failed: {error}", worktree.branch));
                continue;
            }
        };
        match agents_to_close(&worktree.path, workspace_id.as_deref()) {
            Ok(agents) if agents.is_empty() => {}
            // Only a worktree with a proof of the end closes its idle
            // agents: the age alone keeps them.
            Ok(agents) if leftover::closes_idle_agents(worktree.reason, &worktree.facts) => {
                if let Err(error) = close_agents(&agents, &worktree.path) {
                    kept.push(format!("{}: {error}", worktree.branch));
                    continue;
                }
                closed.extend(
                    agents
                        .iter()
                        .map(|agent| format!("{}: {}", worktree.branch, agent.resume_command())),
                );
            }
            Ok(_) => {
                kept.push(format!("{}: an idle agent is in it", worktree.branch));
                continue;
            }
            Err(error) => {
                kept.push(format!("{}: {error}", worktree.branch));
                continue;
            }
        }
        match wt::remove_quietly(&worktree.main_checkout, &worktree.path) {
            Ok(()) => {
                deleted += 1;
                if let (Some(workspace_id), Some(checkout)) =
                    (&workspace_id, git::checkout_of(&worktree.main_checkout))
                {
                    let _ = close_after_removal(workspace_id, &checkout);
                }
            }
            Err(_) => kept.push(format!("{}: wt refused it", worktree.branch)),
        }
    }
    let mut summary = format!("Deleted {}.", worktrees_text(deleted));
    if !closed.is_empty() {
        summary.push_str(&format!(
            "\nClosed idle agents, to resume:\n{}",
            closed.join("\n")
        ));
    }
    if !kept.is_empty() {
        summary.push_str(&format!("\nKept:\n{}", kept.join("\n")));
    }
    ui::show_message("Clean the worktrees", &summary);
    Ok(())
}

/// Popup cleaner: all the linked worktrees, checked in the background.
/// `Enter` deletes one, `ctrl-a` deletes the group "To delete", through
/// the popup confirm. Then the list comes back, without the deleted
/// worktrees.
///
/// The list covers `TRUNK_CLEANER_SCOPE`, or all the repositories of the
/// herdr workspaces. After a question, `TRUNK_CLEANER_STATE` gives the
/// results of the last checks: these worktrees need no new check.
pub fn cleaner(stale_days: u32, fetch: bool) -> Result<(), Error> {
    let folder = popup_folder()?;
    let is_return = env::var(RETURN_VARIABLE).as_deref() == Ok(RETURN_TO_CLEANER);
    // The main checkout of the scope: the return after a question needs a
    // folder that stays, also if the user deletes the folder of the scope.
    let scope = env::var_os(SCOPE_VARIABLE)
        .map(PathBuf::from)
        .map(|scope| {
            git::checkout_of(&scope)
                .map(|checkout| git::canonical(&checkout.main_checkout()))
                .ok_or_else(|| {
                    Error::Command(format!("{} is not in a git repository.", scope.display()))
                })
        })
        .transpose()?;
    let given_state = env::var_os(STATE_VARIABLE)
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let previous = given_state
        .as_deref()
        .map(cleaner::read_state)
        .unwrap_or_default();
    let state_path = given_state.unwrap_or_else(|| {
        env::temp_dir().join(format!(
            "herdr-worktrunk-cleaner-{}.tsv",
            std::process::id()
        ))
    });
    let entries = cleaner::list_worktrees(scope.as_deref(), &previous)?;
    if entries.is_empty() {
        let _ = std::fs::remove_file(&state_path);
        if !is_return {
            let message = match &scope {
                Some(scope) => format!(
                    "No worktree to clean: {} has no linked worktree.",
                    git::folder_name(scope)
                ),
                None => "No worktree to clean: the repositories of the herdr workspaces have no linked worktree.".to_owned(),
            };
            ui::show_message("Clean the worktrees", &message);
        }
        return Ok(());
    }
    let folder_text = folder.to_string_lossy();
    let state_text = state_path.to_string_lossy();
    let scope_text = scope
        .as_ref()
        .map(|scope| scope.to_string_lossy().into_owned());
    let mut environment = vec![
        (STATE_VARIABLE, state_text.as_ref()),
        (RETURN_VARIABLE, RETURN_TO_CLEANER),
        (FOLDER_VARIABLE, folder_text.as_ref()),
    ];
    if let Some(scope_text) = &scope_text {
        environment.push((SCOPE_VARIABLE, scope_text.as_str()));
    }
    match cleaner::choose(entries, &state_path, stale_days, fetch)? {
        None => {
            cleaner::stop_checks();
            let _ = std::fs::remove_file(&state_path);
            Ok(())
        }
        Some(cleaner::Choice::DeleteAll) => {
            // The group To delete stays as the user saw it: the checks write
            // the state file no more.
            cleaner::stop_checks();
            environment.push((OPERATION_VARIABLE, DELETE_ALL));
            open_after_this_popup("confirm", &folder, &environment)
        }
        Some(cleaner::Choice::Delete(entry)) => {
            let reason = entry.question();
            let checkout_text = entry.path.to_string_lossy();
            environment.push((OPERATION_VARIABLE, DELETE));
            environment.push((CHECKOUT_VARIABLE, checkout_text.as_ref()));
            environment.push((REASON_VARIABLE, reason.as_str()));
            if leftover::closes_idle_agents(entry.reason, &entry.facts) {
                environment.push((CLOSE_IDLE_VARIABLE, "1"));
            }
            open_after_this_popup("confirm", &folder, &environment)
        }
    }
}

/// Open the popup cleaner for the daily pass of the observer. herdr shows
/// one popup at a time: wait while another popup is open, for 10 minutes
/// at most.
pub fn open_cleaner(folder: &Path) -> Result<(), Error> {
    let folder_text = folder.to_string_lossy();
    let environment = [(FOLDER_VARIABLE, folder_text.as_ref())];
    for _ in 0..ASK_ATTEMPTS {
        match herdr::open_plugin_pane("cleaner", folder, &environment) {
            Err(Error::Herdr { code, .. }) if code == "ui_busy" => thread::sleep(ASK_INTERVAL),
            other => return other,
        }
    }
    Err(Error::Command(
        "another popup stayed open for 10 minutes".to_owned(),
    ))
}

/// Open the popup cleaner from a shell (command clean): for the
/// repository of `scope`, or for all the repositories of the herdr
/// workspaces.
pub fn open_cleaner_from_shell(scope: Option<&Path>) -> Result<(), Error> {
    let folder = env::current_dir().map_err(|error| Error::Command(error.to_string()))?;
    let folder_text = folder.to_string_lossy();
    let scope_text = scope.map(|scope| scope.to_string_lossy().into_owned());
    let mut environment = vec![(FOLDER_VARIABLE, folder_text.as_ref())];
    if let Some(scope_text) = &scope_text {
        environment.push((SCOPE_VARIABLE, scope_text.as_str()));
    }
    herdr::open_plugin_pane("cleaner", &folder, &environment)
}

/// After a removal: close the workspace of the removed worktree at once.
/// If this workspace has the focus, the focus goes first to the main
/// checkout (else herdr gives it to another workspace).
fn close_after_removal(workspace_id: &str, checkout: &Checkout) -> Result<(), Error> {
    if herdr::workspace_is_focused(workspace_id)? {
        let main_checkout = checkout.main_checkout();
        herdr::open_checkout(&main_checkout, &main_checkout, true)?;
    }
    match herdr::close_workspace(workspace_id) {
        // The observer can close the workspace first.
        Err(Error::Herdr { code, .. }) if code == "workspace_not_found" => Ok(()),
        other => other,
    }
}

/// Popup merger: merge the worktree with wt, then delete it. The output
/// of wt stays on the screen until a key.
pub fn merger() -> Result<(), Error> {
    let checkout_path = env::var_os(CHECKOUT_VARIABLE)
        .map(PathBuf::from)
        .ok_or_else(|| Error::Command("No worktree: run this popup from its action.".to_owned()))?;
    let checkout = git::checkout_of(&checkout_path).ok_or_else(|| {
        Error::Command(format!(
            "{} is not a git checkout.",
            checkout_path.display()
        ))
    })?;
    let workspace_id = env::var(WORKSPACE_VARIABLE).ok();
    let result = wt::merge(&checkout.root)
        .and_then(|()| wt::remove(&checkout.main_checkout(), &checkout.root))
        .and_then(|()| match &workspace_id {
            Some(workspace_id) => close_after_removal(workspace_id, &checkout),
            None => Ok(()),
        });
    match result {
        Ok(()) => ui::wait_after_output("Merged."),
        Err(error) => ui::wait_after_output(&error.to_string()),
    }
    Ok(())
}
