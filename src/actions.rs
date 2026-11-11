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
use crate::restore;
use crate::restorer;
use crate::stale;
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
/// A deletion that the user chose in the popup cleaner: the agents of the
/// worktree close first, after the question.
const CLOSE_AGENTS_VARIABLE: &str = "TRUNK_CLOSE_AGENTS";
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
    if entrypoint == "restorer" {
        // Without a record, the small popup tells it at once.
        if restore::all(&crate::state_dir()).is_empty() {
            return herdr::open_plugin_pane("restoring", folder_path, &[]);
        }
        return herdr::open_plugin_pane("restorer", folder_path, &[]);
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
    // A deletion that the user chose in the cleaner closes the agents of
    // the worktree first. The actions remove and merge refuse an agent.
    let close_agents_first = operation != MERGE && env::var_os(CLOSE_AGENTS_VARIABLE).is_some();
    let agents = if close_agents_first {
        let snapshot = herdr::snapshot()?;
        let agents = leftover::agents_of(&snapshot, &checkout.root, workspace_id.as_deref());
        // wt refuses changes that are not committed: the agents must not
        // close for nothing.
        if !agents.is_empty() && !git::is_clean(&checkout.root) {
            return Err(Error::Command(
                "This worktree has changes that are not committed. Commit or delete them first: its agent stays."
                    .to_owned(),
            ));
        }
        agents
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
    if !agents.is_empty() {
        question.push_str(&agents_text(&agents));
    }
    if !ui::confirm("Delete the worktree", &question, "Delete", true) {
        return Ok(());
    }
    ui::show_progress(
        "Delete the worktree",
        &format!("Deleting the worktree of {branch}…"),
    );
    let removal = record_removal(
        &checkout.main_checkout(),
        &checkout.root,
        &agents,
        workspace_id.as_deref(),
    );
    // wt first: this popup can run in the workspace of the worktree, and
    // the close of the workspace stops it. wt refuses changes that are not
    // committed: then nothing closes.
    if let Err(error) = wt::remove_quietly(&checkout.main_checkout(), &checkout.root) {
        if let Some(removal) = &removal {
            restore::forget(&crate::state_dir(), removal);
        }
        return Err(error);
    }
    // An agent can start after the question: read the agents again.
    let agents = herdr::snapshot()
        .map(|snapshot| leftover::agents_of(&snapshot, &checkout.root, workspace_id.as_deref()))
        .unwrap_or(agents);
    close_agents_elsewhere(&agents);
    if let Some(workspace_id) = &workspace_id {
        close_after_removal(workspace_id, &checkout)?;
    }
    Ok(())
}

/// Close the agents of a removed worktree whose pane is in another
/// workspace. The close of the workspace of the worktree closes the
/// others. A failed close leaves an agent in a removed folder.
fn close_agents_elsewhere(agents: &[WorktreeAgent]) {
    for agent in agents.iter().filter(|agent| !agent.in_own_workspace) {
        let _ = herdr::close_pane(&agent.pane_id);
    }
}

/// Record the removal of the worktree `path` before it starts, for the
/// action restore: its branch, the commit of its branch, and its agents
/// with their sessions. A worktree without a branch (detached HEAD) cannot
/// come back.
fn record_removal(
    main_checkout: &Path,
    path: &Path,
    agents: &[WorktreeAgent],
    workspace_id: Option<&str>,
) -> Option<restore::Removal> {
    let branch = git::branch_of(path)?;
    let commit = git::branch_commit(main_checkout, &branch)?;
    let removal = restore::Removal {
        time: stale::now(),
        main_checkout: main_checkout.to_owned(),
        path: path.to_owned(),
        branch,
        commit,
        agents: agents
            .iter()
            .filter_map(|agent| restore::RemovedAgent::of(&agent.kind, agent.session.as_deref()))
            .collect(),
        recover_command: None,
        tabs: workspace_id.map(restore::capture_tabs).unwrap_or_default(),
    };
    restore::record(&crate::state_dir(), &removal);
    Some(removal)
}

/// Popup restorer: bring back the last removed worktree, then wait for a
/// key. The output of wt (its hooks) stays on the screen.
pub fn restorer() -> Result<(), Error> {
    let removals = restore::all(&crate::state_dir());
    let Some(removal) = restorer::choose(&removals)? else {
        return Ok(());
    };
    // The restore runs in a small popup: its steps and its summary have
    // the size of the other messages.
    let time = removal.time.to_string();
    let path = removal.path.to_string_lossy();
    open_after_this_popup(
        "restoring",
        &popup_folder()?,
        &[
            (RESTORE_TIME_VARIABLE, time.as_str()),
            (RESTORE_PATH_VARIABLE, path.as_ref()),
        ],
    )
}

/// The record that the popup restoring brings back: its time and its
/// folder.
const RESTORE_TIME_VARIABLE: &str = "TRUNK_RESTORE_TIME";
const RESTORE_PATH_VARIABLE: &str = "TRUNK_RESTORE_PATH";

/// Popup restoring: bring back the record of the popup restorer, with a
/// box for each step, then the summary with an OK button. Without a
/// record, tell it.
pub fn restore_run() -> Result<(), Error> {
    let state_dir = crate::state_dir();
    let removals = restore::all(&state_dir);
    let time = env::var(RESTORE_TIME_VARIABLE).unwrap_or_default();
    let path = env::var(RESTORE_PATH_VARIABLE).unwrap_or_default();
    let message = match restorer::find(&removals, &time, &path) {
        None if removals.is_empty() => "No removed worktree to restore.".to_owned(),
        None => "This removed worktree has no record anymore.".to_owned(),
        Some(removal) => match restore_worktree(&state_dir, removal) {
            Ok(message) => message,
            Err(error) => error.to_string(),
        },
    };
    ui::show_message(RESTORE_TITLE, &message);
    Ok(())
}

/// The title of the boxes of the restore.
const RESTORE_TITLE: &str = "Restore a worktree";

/// Bring back one removed worktree: its branch, its worktree, its herdr
/// workspace, and its agents. A box tells each step. Return the summary
/// for the user.
fn restore_worktree(state_dir: &Path, removal: &restore::Removal) -> Result<String, Error> {
    let branch = &removal.branch;
    if removal.path.exists() {
        restore::forget(state_dir, removal);
        return Ok(format!(
            "The worktree of {branch} is already there:\n{}",
            home_relative(&removal.path)
        ));
    }
    if let Some(other) = git::linked_worktrees(&removal.main_checkout)
        .unwrap_or_default()
        .into_iter()
        .find(|worktree| worktree.branch == *branch)
    {
        restore::forget(state_dir, removal);
        return Ok(format!(
            "The branch {branch} is already in a worktree:\n{}",
            home_relative(&other.path)
        ));
    }
    ui::show_progress(
        RESTORE_TITLE,
        &format!("The worktree of {branch} comes back…"),
    );
    let mut lines = Vec::new();
    if git::branch_commit(&removal.main_checkout, branch).is_none() {
        if let Err(error) = git::create_branch(&removal.main_checkout, branch, &removal.commit) {
            let recover = removal
                .recover_command
                .as_deref()
                .map(|command| format!("\nTo get it from the forge: {command}"))
                .unwrap_or_default();
            return Err(Error::Command(format!(
                "Cannot create the branch {branch} on its commit {}: {error}{recover}",
                short_commit(&removal.commit)
            )));
        }
        lines.push(format!(
            "✓ Branch {branch}, again on its commit {}",
            short_commit(&removal.commit)
        ));
    } else {
        lines.push(format!("✓ Branch {branch}"));
    }
    // The guard of the observer comes before the worktree: a pass of the
    // observer between the 2 steps must not remove it again.
    if let Some(commit) = git::branch_commit(&removal.main_checkout, branch) {
        restore::mark_restored(state_dir, &git::canonical(&removal.path), &commit);
    }
    let path = wt::switch_quietly(&removal.main_checkout, branch)?;
    // The observer compares the real path of the worktree: mark it too.
    if let Some(commit) = git::branch_commit(&removal.main_checkout, branch) {
        restore::mark_restored(state_dir, &git::canonical(&path), &commit);
    }
    lines.push(format!("✓ Worktree {}", home_relative(&path)));
    // The post-start hook of wt opens the workspace in the background:
    // wait for it, then open it if the hook did not.
    let mut workspace_id = None;
    for _ in 0..10 {
        workspace_id = herdr::workspace_of_checkout(&path)?;
        if workspace_id.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let workspace_id = match workspace_id {
        Some(workspace_id) => workspace_id,
        None => herdr::open_checkout(&path, &removal.main_checkout, false)?.workspace_id,
    };
    if removal.tabs.is_empty() {
        lines.push("✓ herdr workspace".to_owned());
    } else {
        ui::show_progress(RESTORE_TITLE, "The tabs and the panes come back…");
        match rebuild_tabs(&workspace_id, &path, &removal.tabs) {
            Ok((panes, mut agent_lines)) => {
                lines.push(format!(
                    "✓ {}, {}",
                    count_text(removal.tabs.len(), "tab", "tabs"),
                    count_text(panes, "pane", "panes")
                ));
                lines.append(&mut agent_lines);
            }
            Err(error) => lines.push(format!("✗ tabs: {error}")),
        }
    }
    for agent in removal.agents_without_pane() {
        ui::show_progress(
            RESTORE_TITLE,
            &format!("The {} agent comes back…", agent.kind),
        );
        let result = herdr::open_shell_tab_pane(&workspace_id, &path, "")
            .and_then(|pane_id| resume_agent(agent, &pane_id));
        lines.push(agent_line(agent, result));
    }
    // The record goes only now: an error before keeps it for a new try.
    restore::forget(state_dir, removal);
    lines.push(String::new());
    lines.push("The observer keeps this worktree for 1 day.".to_owned());
    Ok(lines.join("\n"))
}

/// Build the removed tabs again in the workspace `workspace_id`: the first
/// tab takes the tab that the hook of wt opened. Return the number of
/// panes, and one line for each agent.
fn rebuild_tabs(
    workspace_id: &str,
    worktree: &Path,
    tabs: &[restore::RemovedTab],
) -> Result<(usize, Vec<String>), Error> {
    let first_tab = herdr::tabs(workspace_id)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Command("the workspace has no tab".to_owned()))?;
    let first_pane = herdr::snapshot()?
        .panes
        .into_iter()
        .find(|pane| pane.tab_id == first_tab.tab_id)
        .map(|pane| pane.pane_id)
        .ok_or_else(|| Error::Command("the first tab has no pane".to_owned()))?;
    let mut pane_count = 0;
    let mut agent_lines = Vec::new();
    for (index, tab) in tabs.iter().enumerate() {
        let plan = restore::plan_tab(tab);
        // A folder that is gone (not in git) gives the folder of the
        // worktree.
        let folder_of = |pane: usize| {
            plan.panes[pane]
                .as_ref()
                .map(|pane| pane.cwd.clone())
                .filter(|cwd| cwd.exists())
                .unwrap_or_else(|| worktree.to_owned())
        };
        let root = if index == 0 {
            if !tab.label.is_empty() {
                herdr::rename_tab(&first_tab.tab_id, &tab.label)?;
            }
            first_pane.clone()
        } else {
            herdr::open_shell_tab_pane(workspace_id, &folder_of(0), &tab.label)?
        };
        let mut pane_ids = vec![None; plan.panes.len()];
        pane_ids[0] = Some(root);
        for step in &plan.steps {
            let Some(parent) = pane_ids[step.parent].clone() else {
                continue;
            };
            let pane =
                herdr::split_pane(&parent, &step.direction, step.ratio, &folder_of(step.child))?;
            pane_ids[step.child] = Some(pane);
        }
        pane_count += pane_ids.iter().flatten().count();
        for (pane, planned) in plan.panes.iter().enumerate() {
            let (Some(agent), Some(pane_id)) = (
                planned.as_ref().and_then(|planned| planned.agent.as_ref()),
                &pane_ids[pane],
            ) else {
                continue;
            };
            ui::show_progress(
                RESTORE_TITLE,
                &format!("The {} agent comes back…", agent.kind),
            );
            agent_lines.push(agent_line(agent, resume_agent(agent, pane_id)));
        }
    }
    Ok((pane_count, agent_lines))
}

/// Start `agent` again in the shell of `pane_id`, with its conversation.
fn resume_agent(agent: &restore::RemovedAgent, pane_id: &str) -> Result<(), Error> {
    let arguments = agent.resume_arguments().ok_or_else(|| {
        Error::Command("herdr-worktrunk cannot resume this kind of agent".to_owned())
    })?;
    herdr::start_agent(&agent.herdr_name(), &agent.kind, pane_id, &arguments)
}

/// The line of the summary for an agent.
fn agent_line(agent: &restore::RemovedAgent, result: Result<(), Error>) -> String {
    match result {
        Ok(()) => format!("✓ {} agent, with its conversation", agent.kind),
        Err(error) => format!("✗ {} agent: {error}", agent.kind),
    }
}

/// A number with its word: `1 tab`, `3 tabs`.
fn count_text(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// The first 8 characters of a commit.
fn short_commit(commit: &str) -> String {
    commit.chars().take(8).collect()
}

/// The lines of the question about the agents that the deletion closes:
/// a warning for an agent that works, then the command that resumes each
/// agent.
fn agents_text(agents: &[WorktreeAgent]) -> String {
    let mut text = if agents.iter().all(WorktreeAgent::is_idle) {
        "\nIts idle agent closes first.".to_owned()
    } else {
        "\nCareful: an agent works in it now. The deletion closes it.".to_owned()
    };
    for agent in agents {
        text.push_str(&format!("\nTo resume it: {}", agent.resume_command()));
    }
    text
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
    let (old, total) = cleaner::chosen_leftovers(&state_path);
    if old.is_empty() {
        return Ok(());
    }
    // A short question: it must fit in the small popup.
    let names: Vec<String> = old.iter().map(|worktree| worktree.branch.clone()).collect();
    let mut lines = vec![format!(
        "Delete {} of the group To delete?",
        worktrees_text(old.len())
    )];
    lines.extend(name_lines(&names, ui::text_width(), 2));
    // Only the group To delete: the kept worktrees never go.
    if total > old.len() {
        let hidden = total - old.len();
        lines.push(format!(
            "The filter hides {hidden} other{}: {}.",
            if hidden == 1 { "" } else { "s" },
            if hidden == 1 { "it stays" } else { "they stay" }
        ));
    }
    lines.push("wt keeps the branches that are not merged.".to_owned());
    let question = lines.join("\n");
    if !ui::confirm("Clean the worktrees", &question, "Delete", true) {
        return Ok(());
    }
    let mut deleted = 0;
    let mut kept = Vec::new();
    let mut closed = Vec::new();
    // This popup runs in the focused workspace: its worktree goes last,
    // because the close of this workspace stops the popup.
    let focused: Option<String> = herdr::snapshot().ok().and_then(|snapshot| {
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.focused)
            .map(|workspace| workspace.workspace_id.clone())
    });
    let mut ordered: Vec<(&cleaner::Entry, Option<String>)> = Vec::new();
    for worktree in &old {
        match herdr::workspace_of_checkout(&worktree.path) {
            Ok(workspace_id) => ordered.push((worktree, workspace_id)),
            Err(error) => kept.push(format!("{}: herdr failed: {error}", worktree.branch)),
        }
    }
    ordered.sort_by_key(|(_, workspace_id)| focused.is_some() && *workspace_id == focused);
    for (worktree, workspace_id) in ordered {
        ui::show_progress(
            "Clean the worktrees",
            &format!("Deleting the worktree of {}…", worktree.branch),
        );
        // An error keeps this worktree, and the loop goes on: the summary
        // tells what happened to each worktree.
        let agents = match agents_to_close(&worktree.path, workspace_id.as_deref()) {
            Ok(agents) if agents.is_empty() => agents,
            // Only a worktree with a proof of the end closes its idle
            // agents: the age alone keeps them.
            Ok(agents) if leftover::closes_idle_agents(worktree.reason, &worktree.facts) => agents,
            Ok(_) => {
                kept.push(format!("{}: an idle agent is in it", worktree.branch));
                continue;
            }
            Err(error) => {
                kept.push(format!("{}: {error}", worktree.branch));
                continue;
            }
        };
        let removal = record_removal(
            &worktree.main_checkout,
            &worktree.path,
            &agents,
            workspace_id.as_deref(),
        );
        // wt first: the close of the workspace closes the agents, and can
        // stop this popup.
        if wt::remove_quietly(&worktree.main_checkout, &worktree.path).is_err() {
            if let Some(removal) = &removal {
                restore::forget(&crate::state_dir(), removal);
            }
            kept.push(format!("{}: wt refused it", worktree.branch));
            continue;
        }
        deleted += 1;
        close_agents_elsewhere(&agents);
        let is_closed = match (&workspace_id, git::checkout_of(&worktree.main_checkout)) {
            (Some(workspace_id), Some(checkout)) => {
                close_after_removal(workspace_id, &checkout).is_ok()
            }
            (None, _) => true,
            (Some(_), None) => false,
        };
        if is_closed {
            closed.extend(
                agents
                    .iter()
                    .map(|agent| format!("{}: {}", worktree.branch, agent.resume_command())),
            );
        } else if !agents.is_empty() {
            kept.push(format!(
                "{}: deleted, but its workspace and its agents stay open",
                worktree.branch
            ));
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
    // A long summary goes to a file: the popup is small.
    if !ui::message_fits(&summary) {
        let path = crate::state_dir().join("cleaner-summary.txt");
        let mut short = format!("Deleted {}.", worktrees_text(deleted));
        if !kept.is_empty() {
            short.push_str(&format!(" Kept {}.", worktrees_text(kept.len())));
        }
        if !closed.is_empty() {
            short.push_str(&format!(" Closed {} idle agents.", closed.len()));
        }
        summary = match std::fs::create_dir_all(crate::state_dir())
            .and_then(|()| std::fs::write(&path, format!("{summary}\n")))
        {
            Ok(()) => format!("{short}\nThe full list:\n{}", home_relative(&path)),
            Err(_) => short,
        };
    }
    ui::show_message("Clean the worktrees", &summary);
    Ok(())
}

/// A path with `~` for the home folder: shorter in a popup.
fn home_relative(path: &Path) -> String {
    match env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap_or(path).display())
        }
        _ => path.display().to_string(),
    }
}

/// The names of `names`, separated by commas, on `max_lines` lines of
/// `width` characters. The names that do not fit become `… and N more`.
fn name_lines(names: &[String], width: usize, max_lines: usize) -> Vec<String> {
    let length = |text: &str| text.chars().count();
    let mut lines: Vec<String> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let rest = names.len() - index - 1;
        let suffix = if rest > 0 {
            length(&format!(", … and {rest} more"))
        } else {
            0
        };
        let on_last = lines
            .last()
            .map(|line| length(line) + 2 + length(name))
            .filter(|&line_length| line_length <= width);
        let (line_count, line_length) = match on_last {
            Some(line_length) => (lines.len(), line_length),
            None => (lines.len() + 1, length(name)),
        };
        // The last line keeps room for `… and N more`.
        let fits =
            line_count < max_lines || (line_count == max_lines && line_length + suffix <= width);
        if !fits {
            let more = format!("… and {} more", names.len() - index);
            match lines.last_mut() {
                Some(line) => {
                    line.push_str(", ");
                    line.push_str(&more);
                }
                None => lines.push(more),
            }
            return lines;
        }
        match (on_last, lines.last_mut()) {
            (Some(_), Some(line)) => {
                line.push_str(", ");
                line.push_str(name);
            }
            _ => lines.push(name.clone()),
        }
    }
    lines
}

/// Popup cleaner: all the linked worktrees, checked in the background.
/// `Enter` deletes one, `ctrl-a` deletes the worktrees of the group "To
/// delete" that the filter shows, through the popup confirm. Then the list
/// comes back, without the deleted
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
            environment.push((CLOSE_AGENTS_VARIABLE, "1"));
            open_after_this_popup("confirm", &folder, &environment)
        }
    }
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
        .and_then(|()| {
            // The observer can remove the merged worktree first.
            if !checkout.root.exists() {
                return Ok(());
            }
            let removal = record_removal(
                &checkout.main_checkout(),
                &checkout.root,
                &[],
                workspace_id.as_deref(),
            );
            match wt::remove(&checkout.main_checkout(), &checkout.root) {
                Err(_) if !checkout.root.exists() => Ok(()),
                Err(error) => {
                    if let Some(removal) = &removal {
                        restore::forget(&crate::state_dir(), removal);
                    }
                    Err(error)
                }
                ok => ok,
            }
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_of_delete_all_fit_in_2_lines() {
        let names = |count: usize| -> Vec<String> {
            (1..=count)
                .map(|number| format!("feat/branch-{number:02}"))
                .collect()
        };

        assert_eq!(
            name_lines(&names(3), 58, 2),
            ["feat/branch-01, feat/branch-02, feat/branch-03"]
        );
        let many = name_lines(&names(40), 58, 2);
        assert_eq!(many.len(), 2);
        assert!(
            many.iter().all(|line| line.chars().count() <= 58),
            "{many:?}"
        );
        assert!(many[1].ends_with("more"), "{many:?}");
        let shown = many.join(", ").matches("feat/branch-").count();
        assert!(
            many[1].ends_with(&format!("… and {} more", 40 - shown)),
            "{many:?}"
        );
        // A name longer than the width: … and N more on its own line.
        assert_eq!(
            name_lines(&["a".repeat(70), "b".to_owned()], 58, 1),
            ["… and 2 more"]
        );
    }

    #[test]
    fn the_question_tells_which_agents_the_deletion_closes() {
        let agent = |status: &str, session: &str| WorktreeAgent {
            pane_id: "w1:p1".to_owned(),
            kind: "claude".to_owned(),
            session: Some(session.to_owned()),
            status: Some(status.to_owned()),
            works_in_folder: true,
            in_own_workspace: true,
            workspace_focused: false,
        };

        assert_eq!(
            agents_text(&[agent("idle", "s1")]),
            "\nIts idle agent closes first.\nTo resume it: claude --resume s1"
        );
        assert_eq!(
            agents_text(&[agent("idle", "s1"), agent("working", "s2")]),
            "\nCareful: an agent works in it now. The deletion closes it.\nTo resume it: claude --resume s1\nTo resume it: claude --resume s2"
        );
    }
}
