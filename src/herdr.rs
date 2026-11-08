//! The calls to the herdr CLI.
//!
//! Each herdr command prints JSON. On success, the JSON has a `result`
//! object. On failure, herdr exits with 1 and prints `{"error": {"code",
//! "message"}}` on stderr.

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

/// The ID of this plugin in herdr-plugin.toml.
pub const PLUGIN_ID: &str = "herdr-worktrunk";

/// A failure of a herdr command, or of another command of this plugin.
#[derive(Debug)]
pub enum Error {
    /// herdr refused a command.
    Herdr { code: String, message: String },
    /// A command did not start, failed, or printed an unexpected output.
    Command(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Herdr { code, message } => write!(formatter, "herdr: {code}: {message}"),
            Error::Command(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// A pane of the herdr snapshot.
#[derive(Clone, Debug, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub tab_id: String,
    /// The kind of the agent in the pane (claude, codex...), if any.
    pub agent: Option<String>,
    /// The state of the agent: idle, working, blocked, done or unknown.
    #[serde(default)]
    pub agent_status: Option<String>,
    /// The session of the agent, to resume its conversation.
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    pub cwd: Option<PathBuf>,
    /// The folder of the foreground process: the agent, else the shell.
    pub foreground_cwd: Option<PathBuf>,
    #[serde(default)]
    pub focused: bool,
}

/// The session of an agent, as herdr gives it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AgentSession {
    /// The session ID of the agent.
    pub value: String,
}

impl Pane {
    /// The folder where the process of the pane works.
    pub fn folder(&self) -> Option<&Path> {
        self.foreground_cwd.as_deref().or(self.cwd.as_deref())
    }
}

/// A workspace of the herdr snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: Option<String>,
    #[serde(default)]
    pub pane_count: u32,
    #[serde(default)]
    pub tab_count: u32,
    #[serde(default)]
    pub focused: bool,
}

/// The live state of herdr, in one call.
#[derive(Debug, Deserialize)]
pub struct Snapshot {
    pub workspaces: Vec<Workspace>,
    pub panes: Vec<Pane>,
}

/// The git checkout that herdr shows in a workspace.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorkspaceCheckout {
    pub checkout_path: PathBuf,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

/// A workspace that `herdr worktree open` opened or found.
#[derive(Debug)]
pub struct OpenedWorkspace {
    pub workspace_id: String,
    pub tab_id: String,
    pub root_pane_id: String,
}

/// The herdr executable: herdr gives its path to the plugin commands.
fn herdr_executable() -> PathBuf {
    match env::var_os("HERDR_BIN_PATH") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from("herdr"),
    }
}

/// Run a herdr command and return the `result` of its JSON.
fn run(arguments: &[&str]) -> Result<Value, Error> {
    let output = Command::new(herdr_executable())
        .args(arguments)
        .output()
        .map_err(|error| Error::Command(format!("cannot run herdr: {error}")))?;
    if !output.status.success() {
        return Err(error_of(&output.stderr));
    }
    let json: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        Error::Command(format!(
            "herdr {}: invalid JSON: {error}",
            arguments.join(" ")
        ))
    })?;
    Ok(json.get("result").cloned().unwrap_or(Value::Null))
}

/// Make the error of a herdr command from its stderr.
fn error_of(stderr: &[u8]) -> Error {
    let text = String::from_utf8_lossy(stderr);
    let parsed: Option<Value> = serde_json::from_str(text.trim()).ok();
    let error = parsed.as_ref().and_then(|json| json.get("error"));
    let field = |name: &str| {
        error
            .and_then(|error| error.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match (field("code"), field("message")) {
        (Some(code), Some(message)) => Error::Herdr { code, message },
        _ => Error::Command(format!("herdr failed: {}", text.trim())),
    }
}

/// Read a JSON value as a typed value, or fail with the name of the command.
fn decode<T: for<'de> Deserialize<'de>>(value: Value, command: &str) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|error| Error::Command(format!("{command}: {error}")))
}

/// Read one string field of a JSON object, or fail with a clear message.
fn string_field(json: &Value, pointer: &str) -> Result<String, Error> {
    json.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Command(format!("herdr gave no {pointer}")))
}

fn focus_flag(focus: bool) -> &'static str {
    if focus {
        "--focus"
    } else {
        "--no-focus"
    }
}

/// Return the live state of herdr.
pub fn snapshot() -> Result<Snapshot, Error> {
    let result = run(&["api", "snapshot"])?;
    decode(result["snapshot"].clone(), "herdr api snapshot")
}

/// Return the git checkout of a workspace, if the workspace is in git.
///
/// herdr gives the checkout in 3 places. In this order:
///
/// 1. The entry of `herdr worktree list --workspace` that herdr opened in
///    the workspace (`open_workspace_id`).
/// 2. `herdr workspace get`. It keeps the checkout of a workspace after
///    the removal of its worktree, but it has no checkout for a workspace
///    that `herdr workspace create` made.
/// 3. The source checkout of `herdr worktree list` (`source_checkout_path`):
///    for example the main checkout of a workspace that
///    `herdr workspace create` made in a repository.
pub fn workspace_checkout(workspace_id: &str) -> Result<Option<WorkspaceCheckout>, Error> {
    let listed = match run(&["worktree", "list", "--workspace", workspace_id]) {
        Ok(result) => Some(result),
        Err(Error::Herdr { code, .. }) if code == "not_git_worktree" => None,
        Err(error) => return Err(error),
    };
    let worktrees = listed
        .as_ref()
        .and_then(|result| result["worktrees"].as_array().cloned())
        .unwrap_or_default();
    let opened_here = worktrees
        .iter()
        .find(|worktree| worktree["open_workspace_id"].as_str() == Some(workspace_id));
    if let Some(worktree) = opened_here {
        return decode(
            serde_json::json!({
                "checkout_path": worktree["path"],
                "is_linked_worktree": worktree["is_linked_worktree"],
            }),
            "herdr worktree list",
        )
        .map(Some);
    }
    let details = run(&["workspace", "get", workspace_id])?;
    let worktree = details["workspace"]["worktree"].clone();
    if !worktree.is_null() {
        return decode(worktree, "herdr workspace get").map(Some);
    }
    let Some(listed) = listed else {
        return Ok(None);
    };
    let source = string_field(&listed, "/source/source_checkout_path")?;
    Ok(Some(WorkspaceCheckout {
        checkout_path: PathBuf::from(source),
        is_linked_worktree: false,
    }))
}

/// Tell if a workspace has the focus in the herdr UI.
pub fn workspace_is_focused(workspace_id: &str) -> Result<bool, Error> {
    let snapshot = snapshot()?;
    let is_focused = snapshot
        .workspaces
        .iter()
        .any(|workspace| workspace.workspace_id == workspace_id && workspace.focused);
    Ok(is_focused)
}

/// Tell if an agent works in a workspace.
pub fn workspace_has_agent(workspace_id: &str) -> Result<bool, Error> {
    let snapshot = snapshot()?;
    let has_agent = snapshot
        .panes
        .iter()
        .any(|pane| pane.workspace_id == workspace_id && pane.agent.is_some());
    Ok(has_agent)
}

/// Return the workspace that herdr opened for the checkout `checkout`.
pub fn workspace_of_checkout(checkout: &Path) -> Result<Option<String>, Error> {
    let checkout_text = checkout.to_string_lossy();
    let result = run(&["worktree", "list", "--cwd", &checkout_text])?;
    let worktrees = result["worktrees"].as_array().cloned().unwrap_or_default();
    let workspace_id = worktrees
        .iter()
        .find(|worktree| worktree["path"].as_str() == Some(checkout_text.as_ref()))
        .and_then(|worktree| worktree["open_workspace_id"].as_str())
        .map(str::to_owned);
    Ok(workspace_id)
}

/// Open (or find) the herdr workspace of `checkout`.
///
/// herdr refuses to start from a linked worktree: `main_checkout` is the
/// main checkout of the repository.
pub fn open_checkout(
    checkout: &Path,
    main_checkout: &Path,
    focus: bool,
) -> Result<OpenedWorkspace, Error> {
    let result = run(&[
        "worktree",
        "open",
        "--cwd",
        &main_checkout.to_string_lossy(),
        "--path",
        &checkout.to_string_lossy(),
        focus_flag(focus),
    ])?;
    Ok(OpenedWorkspace {
        workspace_id: string_field(&result, "/workspace/workspace_id")?,
        tab_id: string_field(&result, "/tab/tab_id")?,
        root_pane_id: string_field(&result, "/root_pane/pane_id")?,
    })
}

/// Move a pane to a new tab of an existing workspace.
pub fn move_pane_to_new_tab(pane_id: &str, workspace_id: &str, focus: bool) -> Result<(), Error> {
    run(&[
        "pane",
        "move",
        pane_id,
        "--new-tab",
        "--workspace",
        workspace_id,
        focus_flag(focus),
    ])?;
    Ok(())
}

/// Move a pane next to `target_pane_id`, in the tab `tab_id`.
pub fn move_pane_next_to(
    pane_id: &str,
    tab_id: &str,
    target_pane_id: &str,
    focus: bool,
) -> Result<(), Error> {
    run(&[
        "pane",
        "move",
        pane_id,
        "--tab",
        tab_id,
        "--split",
        "right",
        "--target-pane",
        target_pane_id,
        focus_flag(focus),
    ])?;
    Ok(())
}

pub fn close_pane(pane_id: &str) -> Result<(), Error> {
    run(&["pane", "close", pane_id])?;
    Ok(())
}

pub fn close_workspace(workspace_id: &str) -> Result<(), Error> {
    run(&["workspace", "close", workspace_id])?;
    Ok(())
}

/// Open a pane of this plugin (an entrypoint of `[[panes]]`).
pub fn open_plugin_pane(
    entrypoint: &str,
    folder: &Path,
    environment: &[(&str, &str)],
) -> Result<(), Error> {
    let folder_text = folder.to_string_lossy();
    let mut arguments = vec![
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN_ID,
        "--entrypoint",
        entrypoint,
        "--cwd",
        &folder_text,
        "--focus",
    ];
    let variables: Vec<String> = environment
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    for variable in &variables {
        arguments.push("--env");
        arguments.push(variable);
    }
    run(&arguments)?;
    Ok(())
}

/// Show a message to the user in a herdr notification.
pub fn notify(message: &str) {
    // A failed notification must not hide the first error: ignore it.
    let _ = run(&["notification", "show", "worktrunk", "--body", message]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_of_reads_the_json_error_of_herdr() {
        let stderr = br#"{"error":{"code":"workspace_not_found","message":"gone"},"id":"cli"}"#;

        let error = error_of(stderr);

        assert!(matches!(error, Error::Herdr { code, .. } if code == "workspace_not_found"));
    }

    #[test]
    fn error_of_keeps_a_text_error() {
        let error = error_of(b"usage: herdr pane move");

        assert!(matches!(error, Error::Command(message) if message.contains("usage")));
    }

    #[test]
    fn snapshot_reads_the_panes_and_the_workspaces() {
        let json = serde_json::json!({
            "workspaces": [{"workspace_id": "w1", "label": "repo", "pane_count": 2, "tab_count": 1}],
            "panes": [{
                "pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude",
                "cwd": "/repo", "foreground_cwd": "/repo.feature", "focused": true
            }]
        });

        let snapshot: Snapshot = decode(json, "test").unwrap();

        assert_eq!(snapshot.workspaces[0].pane_count, 2);
        assert_eq!(snapshot.panes[0].folder(), Some(Path::new("/repo.feature")));
    }
}
