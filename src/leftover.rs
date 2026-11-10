//! The facts of a worktree, layer by layer, and the verdict of the
//! cleaner: to delete, or kept.
//!
//! The layers, from the cheapest to the most specific:
//!
//! 1. Agent (herdr): no agent, idle agents, or an agent that works.
//! 2. Changes (git status): changes that are not committed.
//! 3. Merged (wt): the content of the branch is in the default branch,
//!    for example after a merge or a squash merge. Any forge.
//! 4. Push (git): a branch that was never pushed has no pull request. A
//!    deleted remote branch is a hint only.
//! 5. Pull requests (the backend of the forge): open, merged or closed, and
//!    the proof that a finished pull request contains the last commit.
//! 6. Activity (git): the days since the last commit or checkout.
//!
//! The verdict, in this order:
//!
//! 1. An agent that works, or changes: kept.
//! 2. An open pull request: kept.
//! 3. A finished pull request (merged or closed) that contains the last
//!    commit: to delete.
//! 4. The content is in the default branch, and no activity for 1 day: to
//!    delete.
//! 5. No pull request (the forge says so, or the branch was never pushed
//!    and the forge gives no answer), and no activity for `stale_days`
//!    days: to delete.
//! 6. Else kept: recent activity, work after a finished pull request, or
//!    a forge that gave no answer.
//!
//! Idle agents (idle or done, in the worktree and in its workspace,
//! without the focus) do not keep a worktree with a proof of the end (rules
//! 3 and 4): the deletion closes them first, as the observer does. They
//! keep a worktree of rule 5: the age alone does not prove that the work is
//! finished.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git;
use crate::herdr::Snapshot;
use crate::prune::{BranchStatus, KeptWhy};
use crate::style::{ACCENT, DIM, GREEN, RED, YELLOW};

/// The days without activity before a merged worktree is a leftover: a
/// new worktree has the commit of the default branch, so it is merged at
/// once. `wt step prune` has the same rule.
pub const INTEGRATED_MIN_DAYS: u64 = 1;

/// The agents of a worktree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agents {
    #[default]
    None,
    /// All the agents are idle or done, work in the folder of the worktree
    /// and in its workspace, and the workspace has no focus: a deletion can
    /// close them.
    Idle,
    /// An agent works, waits for an answer, has the focus, or works in
    /// another workspace.
    Busy,
}

/// An agent pane that uses a worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeAgent {
    pub pane_id: String,
    /// The kind of the agent: claude, codex...
    pub kind: String,
    pub session: Option<String>,
    /// idle, working, blocked, done or unknown.
    pub status: Option<String>,
    /// The folder of the agent is in the worktree.
    pub works_in_folder: bool,
    /// The pane is in the workspace of the worktree.
    pub in_own_workspace: bool,
    pub workspace_focused: bool,
}

impl WorktreeAgent {
    /// Return true if a deletion can close this agent.
    fn may_close(&self) -> bool {
        matches!(self.status.as_deref(), Some("idle" | "done"))
            && self.works_in_folder
            && self.in_own_workspace
            && !self.workspace_focused
    }

    /// The command that resumes the conversation of the agent.
    pub fn resume_command(&self) -> String {
        let session = self.session.as_deref().unwrap_or("<session>");
        match self.kind.as_str() {
            "claude" => format!("claude --resume {session}"),
            "codex" => format!("codex resume {session}"),
            kind => format!("{kind}, session {session}"),
        }
    }
}

/// The agent panes that use the worktree `path`: their folder is in the
/// worktree, or their pane is in its workspace (`workspace_id`).
pub fn agents_of(
    snapshot: &Snapshot,
    path: &Path,
    workspace_id: Option<&str>,
) -> Vec<WorktreeAgent> {
    let path = git::canonical(path);
    let focused: HashSet<&str> = snapshot
        .workspaces
        .iter()
        .filter(|workspace| workspace.focused)
        .map(|workspace| workspace.workspace_id.as_str())
        .collect();
    snapshot
        .panes
        .iter()
        .filter_map(|pane| {
            let kind = pane.agent.clone()?;
            let works_in_folder = pane
                .folder()
                .is_some_and(|folder| git::canonical(folder).starts_with(&path));
            let in_own_workspace = workspace_id == Some(pane.workspace_id.as_str());
            (works_in_folder || in_own_workspace).then(|| WorktreeAgent {
                pane_id: pane.pane_id.clone(),
                kind,
                session: pane
                    .agent_session
                    .as_ref()
                    .map(|session| session.value.clone()),
                status: pane.agent_status.clone(),
                works_in_folder,
                in_own_workspace,
                workspace_focused: focused.contains(pane.workspace_id.as_str()),
            })
        })
        .collect()
}

/// The state of the agents of a worktree.
pub fn agents_state(agents: &[WorktreeAgent]) -> Agents {
    if agents.is_empty() {
        Agents::None
    } else if agents.iter().all(WorktreeAgent::may_close) {
        Agents::Idle
    } else {
        Agents::Busy
    }
}

/// The result of `git status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Changes {
    #[default]
    NotChecked,
    None,
    Some,
    /// git status failed.
    Failed,
}

/// The content of the branch in the default branch, from wt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Integration {
    #[default]
    NotChecked,
    /// The branch has commits that the default branch does not have: the
    /// number, if wt gives it.
    NotIntegrated(Option<u64>),
    /// The check of wt that found the content in the default branch, for
    /// example `patch_id_match` for a squash merge.
    Integrated(String),
    /// wt gave no answer, for this reason.
    Undetermined(String),
}

/// Where the branch is on the remotes, from git.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Push {
    #[default]
    NotChecked,
    /// No upstream, and no remote branch of the same name.
    Never,
    /// The remote branch (`origin/feat`).
    Pushed(String),
    /// The upstream is gone: the last fetch removed it. A hint only.
    Gone(String),
}

/// The pull requests of the branch, from the backend of the forge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullRequests {
    #[default]
    NotChecked,
    /// The checks did not ask the forge, for this reason.
    NotNeeded(String),
    None,
    Open(u64),
    /// A finished pull request that contains the last commit.
    Finished {
        number: u64,
        merged: bool,
    },
    /// Finished, but the branch has commits after it.
    WorkAfter(u64),
    /// Finished, and the default branch contains the commit.
    InDefault(u64),
    /// Finished, but the branch has no local commit.
    NoLocalCommit(u64),
    /// The forge gave no answer, for this reason.
    Failed(String),
}

impl PullRequests {
    /// The facts of a status of the forge.
    pub fn from_status(status: Result<BranchStatus, String>) -> PullRequests {
        match status {
            Err(error) => PullRequests::Failed(error),
            Ok(BranchStatus::NoPullRequest) => PullRequests::None,
            Ok(BranchStatus::Open(number)) => PullRequests::Open(number),
            Ok(BranchStatus::Finished { pull_request, .. }) => PullRequests::Finished {
                number: pull_request.number,
                merged: pull_request.state == crate::forge::PullRequestState::Merged,
            },
            Ok(BranchStatus::Kept { number, why }) => match why {
                KeptWhy::WorkAfter => PullRequests::WorkAfter(number),
                KeptWhy::InDefaultBranch => PullRequests::InDefault(number),
                KeptWhy::NoLocalCommit => PullRequests::NoLocalCommit(number),
            },
        }
    }

    /// The number of the pull request, if the facts name one.
    pub fn number(&self) -> Option<u64> {
        match self {
            PullRequests::Open(number)
            | PullRequests::Finished { number, .. }
            | PullRequests::WorkAfter(number)
            | PullRequests::InDefault(number)
            | PullRequests::NoLocalCommit(number) => Some(*number),
            _ => None,
        }
    }
}

/// The facts of a worktree, layer by layer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub agent: Agents,
    pub changes: Changes,
    pub integration: Integration,
    pub push: Push,
    pub pull_requests: PullRequests,
    /// The forge that gave the pull requests, for the user.
    pub forge: String,
    /// The default branch of the repository (`main`), for the user.
    #[serde(default)]
    pub default_branch: Option<String>,
    /// The days since the last activity, once known.
    pub days: Option<u64>,
}

impl Facts {
    /// The name of the default branch, for the texts.
    fn main(&self) -> &str {
        self.default_branch
            .as_deref()
            .unwrap_or("the default branch")
    }
}

/// The verdict of the cleaner for a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// The checks run.
    Checking,
    /// To delete: a merged pull request contains the last commit.
    Merged,
    /// To delete: a closed pull request contains the last commit.
    Closed,
    /// To delete: the content is in the default branch.
    Integrated,
    /// To delete: no pull request, and no activity for `stale_days` days.
    Old,
    /// Kept: an open pull request.
    Open,
    /// Kept: some activity in the last days.
    Recent,
    /// Kept: changes that are not committed.
    Changes,
    /// Kept: an agent works in the worktree.
    Agent,
    /// Kept: an idle agent, and no proof that the work is finished.
    IdleAgent,
    /// Kept: the branch has work after its finished pull request.
    WorkAfter,
    /// Kept: a finished pull request, and the default branch contains the
    /// commit, but wt did not see the merge.
    InDefault,
    /// Kept: a check gave no answer.
    Unknown,
}

impl Reason {
    /// Return true for a leftover: the group "To delete".
    pub fn is_leftover(self) -> bool {
        matches!(
            self,
            Reason::Merged | Reason::Closed | Reason::Integrated | Reason::Old
        )
    }
}

/// The verdict for the facts of a worktree (see the module doc).
pub fn verdict(facts: &Facts, stale_days: u32) -> Reason {
    if facts.agent == Agents::Busy {
        return Reason::Agent;
    }
    let reason = verdict_of_work(facts, stale_days);
    // The age alone does not prove the end of the work of an idle agent.
    if facts.agent == Agents::Idle && reason == Reason::Old {
        return Reason::IdleAgent;
    }
    reason
}

/// The verdict from the layers after the agents.
fn verdict_of_work(facts: &Facts, stale_days: u32) -> Reason {
    match facts.changes {
        Changes::None => {}
        Changes::Some => return Reason::Changes,
        Changes::Failed | Changes::NotChecked => return Reason::Unknown,
    }
    match facts.pull_requests {
        PullRequests::Open(_) => return Reason::Open,
        PullRequests::Finished { merged: true, .. } => return Reason::Merged,
        PullRequests::Finished { merged: false, .. } => return Reason::Closed,
        _ => {}
    }
    let is_integrated = matches!(facts.integration, Integration::Integrated(_));
    if is_integrated && facts.days.is_some_and(|days| days >= INTEGRATED_MIN_DAYS) {
        return Reason::Integrated;
    }
    // A branch that was never pushed has no pull request, but only if the
    // forge gives no answer: a push without -u, or to another name, can
    // leave no remote branch here.
    let forge_is_silent = matches!(
        facts.pull_requests,
        PullRequests::Failed(_) | PullRequests::NotChecked | PullRequests::NotNeeded(_)
    );
    let has_no_pull_request = matches!(facts.pull_requests, PullRequests::None)
        || (matches!(facts.push, Push::Never) && forge_is_silent);
    if has_no_pull_request {
        return match facts.days {
            Some(days) if days >= u64::from(stale_days) => Reason::Old,
            Some(_) => Reason::Recent,
            None => Reason::Unknown,
        };
    }
    match facts.pull_requests {
        PullRequests::WorkAfter(_) => Reason::WorkAfter,
        PullRequests::InDefault(_) => Reason::InDefault,
        _ if is_integrated && facts.days.is_some() => Reason::Recent,
        _ => Reason::Unknown,
    }
}

/// A number of days: `1 day`, `7 days`.
fn days_text(days: u64) -> String {
    if days == 1 {
        "1 day".to_owned()
    } else {
        format!("{days} days")
    }
}

/// A number of commits: `1 commit`, `3 commits`.
fn commits_text(commits: u64) -> String {
    if commits == 1 {
        "1 commit".to_owned()
    } else {
        format!("{commits} commits")
    }
}

/// The words of a check of wt that found the content in the default
/// branch `main`.
fn integration_words(reason: &str, main: &str) -> String {
    match reason {
        "same_commit" => format!("the same commit as {main}"),
        "ancestor" => format!("an older commit of {main}"),
        "no_added_changes" => format!("no change that {main} does not have"),
        "trees_match" => format!("the same files as {main}"),
        "merge_adds_nothing" => format!("a merge into {main} adds nothing"),
        "patch_id_match" => format!("a squash merge into {main}"),
        other => format!("{other} ({main})"),
    }
}

/// The detail of a check that gave no answer, for the row and the
/// question.
pub fn unknown_detail(facts: &Facts) -> String {
    match (&facts.changes, &facts.pull_requests) {
        (Changes::Failed, _) => "git status failed".to_owned(),
        (_, PullRequests::Failed(error)) => error.clone(),
        (_, PullRequests::NoLocalCommit(_)) => "no local commit".to_owned(),
        _ if facts.days.is_none() => "no date of activity".to_owned(),
        _ => "no answer".to_owned(),
    }
}

/// The text of a verdict in a row of the list.
pub fn reason_text(reason: Reason, facts: &Facts) -> String {
    let number = facts
        .pull_requests
        .number()
        .map(|number| format!("#{number}"))
        .unwrap_or_default();
    let never_pushed = matches!(facts.push, Push::Never);
    let text = match reason {
        Reason::Checking => "checking…".to_owned(),
        Reason::Merged => format!("PR {number} merged"),
        Reason::Closed => format!("PR {number} closed"),
        Reason::Integrated => match &facts.integration {
            Integration::Integrated(how) => {
                format!("merged: {}", integration_words(how, facts.main()))
            }
            _ => format!("merged into {}", facts.main()),
        },
        Reason::Old if never_pushed => "never pushed, no recent activity".to_owned(),
        Reason::Old => "no PR, no recent activity".to_owned(),
        Reason::Open => format!("PR {number} open"),
        Reason::Recent if matches!(facts.integration, Integration::Integrated(_)) => {
            "merged, activity today".to_owned()
        }
        Reason::Recent if never_pushed => "never pushed, recent activity".to_owned(),
        Reason::Recent => "no PR, recent activity".to_owned(),
        Reason::Changes => "changes not committed".to_owned(),
        Reason::Agent => "an agent works here".to_owned(),
        Reason::IdleAgent => "an idle agent, no proof of the end".to_owned(),
        Reason::WorkAfter => format!("work after PR {number}"),
        Reason::InDefault => format!("PR {number} finished, commit in the default branch"),
        Reason::Unknown => format!("not checked: {}", unknown_detail(facts)),
    };
    if reason.is_leftover() && facts.agent == Agents::Idle {
        format!("{text}, idle agent closed first")
    } else {
        text
    }
}

/// The text of a verdict in the question of the popup confirm.
pub fn question(reason: Reason, facts: &Facts) -> String {
    let number = facts
        .pull_requests
        .number()
        .map(|number| format!("#{number}"))
        .unwrap_or_default();
    let days = facts.days.unwrap_or(0);
    match reason {
        Reason::Merged => format!("Its pull request {number} is merged."),
        Reason::Closed => format!("Its pull request {number} is closed."),
        Reason::Integrated => format!("{} contains its content.", facts.main()),
        Reason::Old => format!("No pull request, and no change for {days} days."),
        Reason::Open => format!("Careful: its pull request {number} is open."),
        Reason::Recent => format!("No proof that it is finished, and a change {days} days ago."),
        Reason::Changes => "Careful: it has changes that are not committed.".to_owned(),
        Reason::Agent => "Careful: an agent works in it.".to_owned(),
        Reason::IdleAgent => {
            "Careful: an idle agent is in it, and no proof shows the end of its work.".to_owned()
        }
        Reason::WorkAfter => format!("Careful: the branch has work after {number}."),
        Reason::InDefault => format!(
            "Its pull request {number} is finished, and the default branch contains its commit."
        ),
        Reason::Checking | Reason::Unknown => "The cleaner did not check it.".to_owned(),
    }
}

/// The result of a layer in the tab `checks`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The layer allows the deletion.
    Pass,
    /// The layer keeps the worktree.
    Fail,
    /// A fact that does not decide.
    Info,
    /// The check runs.
    Wait,
    /// The check gave no answer.
    Unknown,
}

impl Mark {
    pub fn glyph(self) -> (&'static str, &'static str) {
        match self {
            Mark::Pass => (GREEN, "✓"),
            Mark::Fail => (RED, "✗"),
            Mark::Info => (DIM, "–"),
            Mark::Wait => (ACCENT, "…"),
            Mark::Unknown => (YELLOW, "?"),
        }
    }
}

/// A line of the tab `checks`: the result, the layer, the fact, and the
/// source of the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub mark: Mark,
    pub layer: &'static str,
    pub text: String,
    pub source: String,
}

fn line(mark: Mark, layer: &'static str, text: impl Into<String>, source: &str) -> Line {
    Line {
        mark,
        layer,
        text: text.into(),
        source: source.to_owned(),
    }
}

/// The lines of the tab `checks`, one for each layer, in the order of the
/// checks.
pub fn lines(facts: &Facts, reason: Reason, stale_days: u32) -> Vec<Line> {
    let checking = reason == Reason::Checking;
    let waiting = |layer: &'static str, source: &str| {
        if checking {
            line(Mark::Wait, layer, "checking…", source)
        } else {
            line(Mark::Info, layer, "not needed", source)
        }
    };
    let agent = match facts.agent {
        Agents::None => line(Mark::Pass, "Agent", "none", "herdr"),
        Agents::Busy => line(Mark::Fail, "Agent", "an agent works here", "herdr"),
        Agents::Idle if reason.is_leftover() => line(
            Mark::Pass,
            "Agent",
            "idle: it closes before the deletion",
            "herdr",
        ),
        Agents::Idle if reason == Reason::IdleAgent => line(
            Mark::Fail,
            "Agent",
            "idle, but no proof of the end of its work",
            "herdr",
        ),
        Agents::Idle => line(Mark::Info, "Agent", "idle", "herdr"),
    };
    let changes = match facts.changes {
        Changes::None => line(Mark::Pass, "Changes", "none", "git status"),
        Changes::Some => line(Mark::Fail, "Changes", "not committed", "git status"),
        Changes::Failed => line(Mark::Unknown, "Changes", "git status failed", "git status"),
        Changes::NotChecked => waiting("Changes", "git status"),
    };
    let merged = match &facts.integration {
        Integration::Integrated(how) => line(
            Mark::Pass,
            "Merged",
            format!("yes: {}", integration_words(how, facts.main())),
            "wt",
        ),
        Integration::NotIntegrated(Some(commits)) => line(
            Mark::Info,
            "Merged",
            format!("no: {} not in {}", commits_text(*commits), facts.main()),
            "wt",
        ),
        Integration::NotIntegrated(None) => line(
            Mark::Info,
            "Merged",
            format!("no: commits not in {}", facts.main()),
            "wt",
        ),
        Integration::Undetermined(why) => {
            line(Mark::Unknown, "Merged", format!("unknown: {why}"), "wt")
        }
        Integration::NotChecked => waiting("Merged", "wt"),
    };
    let push = match &facts.push {
        Push::Never => line(
            Mark::Pass,
            "Push",
            "never pushed: no pull request is possible",
            "git",
        ),
        Push::Pushed(remote) => line(Mark::Info, "Push", format!("pushed to {remote}"), "git"),
        Push::Gone(remote) => line(
            Mark::Info,
            "Push",
            format!("{remote} is deleted (a hint only)"),
            "git",
        ),
        Push::NotChecked => waiting("Push", "git"),
    };
    let forge = if facts.forge.is_empty() {
        "forge".to_owned()
    } else {
        facts.forge.clone()
    };
    let pull_requests = match &facts.pull_requests {
        PullRequests::None => line(Mark::Pass, "Pull requests", "none", &forge),
        PullRequests::Finished { number, merged } => line(
            Mark::Pass,
            "Pull requests",
            format!(
                "#{number} {}, with the last commit",
                if *merged { "merged" } else { "closed" }
            ),
            &forge,
        ),
        PullRequests::Open(number) => line(
            Mark::Fail,
            "Pull requests",
            format!("#{number} open"),
            &forge,
        ),
        PullRequests::WorkAfter(number) => line(
            Mark::Fail,
            "Pull requests",
            format!("#{number} finished, but the branch has commits after it"),
            &forge,
        ),
        PullRequests::InDefault(number) => line(
            Mark::Info,
            "Pull requests",
            format!("#{number} finished, the default branch has the commit"),
            &forge,
        ),
        PullRequests::NoLocalCommit(number) => line(
            Mark::Unknown,
            "Pull requests",
            format!("#{number} finished, but no local commit"),
            &forge,
        ),
        PullRequests::NotNeeded(why) => line(
            Mark::Info,
            "Pull requests",
            format!("not needed: {why}"),
            &forge,
        ),
        PullRequests::Failed(error) => line(
            Mark::Unknown,
            "Pull requests",
            format!("no answer: {error}"),
            &forge,
        ),
        PullRequests::NotChecked => waiting("Pull requests", &forge),
    };
    let activity = match facts.days {
        Some(days) => {
            // The rule of the merge has its own limit.
            let limit = if matches!(facts.integration, Integration::Integrated(_)) {
                INTEGRATED_MIN_DAYS
            } else {
                u64::from(stale_days)
            };
            let mark = if days >= limit {
                Mark::Pass
            } else if reason == Reason::Recent {
                Mark::Fail
            } else {
                Mark::Info
            };
            line(
                mark,
                "Activity",
                format!(
                    "{} without activity (limit: {})",
                    days_text(days),
                    days_text(limit)
                ),
                "git",
            )
        }
        None => waiting("Activity", "git"),
    };
    vec![agent, changes, merged, push, pull_requests, activity]
}

/// The name of the default branch, from the JSON of `wt list`.
pub fn parse_default_branch(json: &Value) -> Option<String> {
    json["repo"]["default_branch"].as_str().map(str::to_owned)
}

/// The merge of each worktree in the default branch, from the JSON of
/// `wt list --format json`: `default_branch.integration` is absent when wt
/// found no merge, null when wt could not decide, and an object with the
/// reason when the default branch has the content.
pub fn parse_integrations(json: &Value) -> HashMap<String, Integration> {
    let mut integrations = HashMap::new();
    for item in json["items"].as_array().into_iter().flatten() {
        let Some(branch) = item["branch"].as_str() else {
            continue;
        };
        let default_branch = &item["default_branch"];
        let integration = if item.get("head").is_some_and(Value::is_null) {
            Integration::Undetermined("the branch has no commit".to_owned())
        } else if !default_branch.is_object() {
            Integration::Undetermined("wt gave no answer".to_owned())
        } else if default_branch["orphan"].as_bool() == Some(true) {
            Integration::Undetermined("no common history with the default branch".to_owned())
        } else {
            match default_branch.get("integration") {
                None => Integration::NotIntegrated(default_branch["ahead"].as_u64()),
                Some(Value::Null) => {
                    Integration::Undetermined("wt could not decide in time".to_owned())
                }
                Some(found) => Integration::Integrated(
                    found["reason"].as_str().unwrap_or("integrated").to_owned(),
                ),
            }
        };
        integrations.insert(branch.to_owned(), integration);
    }
    integrations
}

/// The push state of each local branch, from 2 outputs of
/// `git for-each-ref`: the branches (`<branch>\t<upstream>\t<track>`),
/// and the remote branches (`<remote>/<branch>`).
pub fn parse_pushes(branches: &str, remote_branches: &str) -> HashMap<String, Push> {
    let remotes: HashSet<&str> = remote_branches.lines().map(str::trim).collect();
    let mut pushes = HashMap::new();
    for row in branches.lines() {
        let mut fields = row.split('\t');
        let Some(branch) = fields.next().filter(|branch| !branch.is_empty()) else {
            continue;
        };
        let upstream = fields.next().unwrap_or_default();
        let track = fields.next().unwrap_or_default();
        let push = if !upstream.is_empty() {
            if track.contains("gone") {
                Push::Gone(upstream.to_owned())
            } else {
                Push::Pushed(upstream.to_owned())
            }
        } else {
            // A push without -u gives no upstream, but a remote branch.
            match remotes.iter().find(|remote| {
                remote
                    .split_once('/')
                    .is_some_and(|(_, name)| name == branch)
            }) {
                Some(remote) => Push::Pushed((*remote).to_owned()),
                None => Push::Never,
            }
        };
        pushes.insert(branch.to_owned(), push);
    }
    pushes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            agent: Agents::None,
            changes: Changes::None,
            integration: Integration::NotIntegrated(Some(2)),
            push: Push::Pushed("origin/feat".to_owned()),
            pull_requests: PullRequests::None,
            forge: "GitHub".to_owned(),
            default_branch: Some("main".to_owned()),
            days: Some(30),
        }
    }

    #[test]
    fn the_verdict_follows_the_order_of_the_layers() {
        let squash = Integration::Integrated("patch_id_match".to_owned());
        let merged_pr = PullRequests::Finished {
            number: 3,
            merged: true,
        };
        let no_backend = PullRequests::Failed("no backend for GitLab yet".to_owned());
        let cases: Vec<(&str, Facts, Reason)> = vec![
            (
                "busy agent",
                Facts {
                    agent: Agents::Busy,
                    ..facts()
                },
                Reason::Agent,
            ),
            (
                "changes",
                Facts {
                    changes: Changes::Some,
                    ..facts()
                },
                Reason::Changes,
            ),
            (
                "git status failed",
                Facts {
                    changes: Changes::Failed,
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "an open PR wins over the merge",
                Facts {
                    pull_requests: PullRequests::Open(3),
                    integration: squash.clone(),
                    ..facts()
                },
                Reason::Open,
            ),
            (
                "merged PR",
                Facts {
                    pull_requests: merged_pr.clone(),
                    days: Some(0),
                    ..facts()
                },
                Reason::Merged,
            ),
            (
                "closed PR",
                Facts {
                    pull_requests: PullRequests::Finished {
                        number: 3,
                        merged: false,
                    },
                    ..facts()
                },
                Reason::Closed,
            ),
            (
                "merged, 1 day without activity, the forge failed",
                Facts {
                    integration: squash.clone(),
                    pull_requests: PullRequests::Failed("gh: no end".to_owned()),
                    days: Some(1),
                    ..facts()
                },
                Reason::Integrated,
            ),
            (
                "a new worktree is merged today: kept",
                Facts {
                    integration: Integration::Integrated("same_commit".to_owned()),
                    pull_requests: PullRequests::NotNeeded("no remote origin".to_owned()),
                    push: Push::Never,
                    days: Some(0),
                    ..facts()
                },
                Reason::Recent,
            ),
            ("no PR and old", facts(), Reason::Old),
            (
                "no PR and recent",
                Facts {
                    days: Some(2),
                    ..facts()
                },
                Reason::Recent,
            ),
            (
                "never pushed, no remote, and old",
                Facts {
                    push: Push::Never,
                    pull_requests: PullRequests::NotNeeded("no remote origin".to_owned()),
                    ..facts()
                },
                Reason::Old,
            ),
            (
                "never pushed here, but the forge has an open PR",
                Facts {
                    push: Push::Never,
                    pull_requests: PullRequests::Open(5),
                    ..facts()
                },
                Reason::Open,
            ),
            (
                "never pushed and no backend: the age decides",
                Facts {
                    push: Push::Never,
                    pull_requests: no_backend.clone(),
                    ..facts()
                },
                Reason::Old,
            ),
            (
                "pushed and no backend: not checked",
                Facts {
                    pull_requests: no_backend.clone(),
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "a deleted remote branch is a hint only",
                Facts {
                    push: Push::Gone("origin/feat".to_owned()),
                    pull_requests: no_backend,
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "the commit is in the default branch, but wt failed",
                Facts {
                    integration: Integration::Undetermined("wt gave no answer".to_owned()),
                    pull_requests: PullRequests::InDefault(4),
                    ..facts()
                },
                Reason::InDefault,
            ),
            (
                "work after a finished PR",
                Facts {
                    pull_requests: PullRequests::WorkAfter(3),
                    ..facts()
                },
                Reason::WorkAfter,
            ),
            (
                "no date of activity",
                Facts {
                    days: None,
                    ..facts()
                },
                Reason::Unknown,
            ),
        ];
        for (name, facts, expected) in cases {
            assert_eq!(verdict(&facts, 7), expected, "{name}");
        }
    }

    #[test]
    fn an_idle_agent_does_not_keep_a_proof_of_the_end_but_keeps_an_old_worktree() {
        let idle = |facts: Facts| Facts {
            agent: Agents::Idle,
            ..facts
        };

        // A finished PR or a merge: the idle agent closes first.
        let merged_pr = idle(Facts {
            pull_requests: PullRequests::Finished {
                number: 3,
                merged: true,
            },
            ..facts()
        });
        assert_eq!(verdict(&merged_pr, 7), Reason::Merged);
        assert_eq!(
            reason_text(Reason::Merged, &merged_pr),
            "PR #3 merged, idle agent closed first"
        );
        let squashed = idle(Facts {
            integration: Integration::Integrated("patch_id_match".to_owned()),
            days: Some(2),
            ..facts()
        });
        assert_eq!(verdict(&squashed, 7), Reason::Integrated);
        // The age alone: the idle agent keeps the worktree.
        assert_eq!(verdict(&idle(facts()), 7), Reason::IdleAgent);
        // An open PR stays an open PR.
        let open = idle(Facts {
            pull_requests: PullRequests::Open(1),
            ..facts()
        });
        assert_eq!(verdict(&open, 7), Reason::Open);
    }

    #[test]
    fn a_failed_forge_never_gives_a_leftover_without_a_proof_of_git() {
        for days in [Some(0), Some(400), None] {
            let facts = Facts {
                pull_requests: PullRequests::Failed("gh pr: no end after 15 s".to_owned()),
                days,
                ..facts()
            };

            assert!(!verdict(&facts, 7).is_leftover(), "{days:?}");
        }
    }

    fn snapshot(panes: Value, focused: &str) -> Snapshot {
        serde_json::from_value(serde_json::json!({
            "workspaces": [
                { "workspace_id": "w1", "label": "app.feat", "focused": focused == "w1" },
                { "workspace_id": "w2", "label": "app", "focused": focused == "w2" },
            ],
            "panes": panes,
        }))
        .unwrap()
    }

    fn agent_pane(pane_id: &str, workspace_id: &str, status: &str, cwd: &str) -> Value {
        serde_json::json!({
            "pane_id": pane_id, "workspace_id": workspace_id, "agent": "claude",
            "agent_status": status, "agent_session": { "value": "s-1" },
            "cwd": cwd, "foreground_cwd": cwd,
        })
    }

    #[test]
    fn the_agents_of_a_worktree_are_idle_only_if_the_deletion_can_close_them() {
        let path = Path::new("/work/app.feat");
        let state = |panes: Value, focused: &str| {
            agents_state(&agents_of(&snapshot(panes, focused), path, Some("w1")))
        };

        assert_eq!(state(serde_json::json!([]), "w2"), Agents::None);
        let idle = serde_json::json!([agent_pane("p1", "w1", "idle", "/work/app.feat/src")]);
        assert_eq!(state(idle.clone(), "w2"), Agents::Idle);
        // The focus keeps the agent.
        assert_eq!(state(idle, "w1"), Agents::Busy);
        let working = serde_json::json!([agent_pane("p1", "w1", "working", "/work/app.feat")]);
        assert_eq!(state(working, "w2"), Agents::Busy);
        // An agent of the worktree in another workspace: not closed.
        let elsewhere = serde_json::json!([agent_pane("p1", "w2", "done", "/work/app.feat")]);
        assert_eq!(state(elsewhere, "w1"), Agents::Busy);
        // An agent in the workspace, but in another folder: not closed.
        let other_folder = serde_json::json!([agent_pane("p1", "w1", "idle", "/work/other")]);
        assert_eq!(state(other_folder, "w2"), Agents::Busy);
        // A pane in another workspace and folder does not count.
        let unrelated = serde_json::json!([agent_pane("p1", "w2", "working", "/work/app")]);
        assert_eq!(state(unrelated, "w2"), Agents::None);
    }

    #[test]
    fn the_resume_command_depends_on_the_kind_of_agent() {
        let agent = WorktreeAgent {
            pane_id: "p1".to_owned(),
            kind: "claude".to_owned(),
            session: Some("abc".to_owned()),
            status: Some("idle".to_owned()),
            works_in_folder: true,
            in_own_workspace: true,
            workspace_focused: false,
        };

        assert_eq!(agent.resume_command(), "claude --resume abc");
        let codex = WorktreeAgent {
            kind: "codex".to_owned(),
            ..agent
        };
        assert_eq!(codex.resume_command(), "codex resume abc");
    }

    #[test]
    fn the_facts_go_to_json_and_back() {
        let facts = Facts {
            agent: Agents::Idle,
            integration: Integration::Integrated("patch_id_match".to_owned()),
            push: Push::Gone("origin/feat".to_owned()),
            pull_requests: PullRequests::Finished {
                number: 12,
                merged: false,
            },
            ..facts()
        };

        let text = serde_json::to_string(&facts).unwrap();

        assert_eq!(serde_json::from_str::<Facts>(&text).unwrap(), facts);
    }

    #[test]
    fn the_tab_checks_has_one_line_for_each_layer() {
        let facts = Facts {
            integration: Integration::Integrated("patch_id_match".to_owned()),
            days: Some(2),
            ..facts()
        };

        let shown = lines(&facts, Reason::Integrated, 7);

        let layers: Vec<&str> = shown.iter().map(|line| line.layer).collect();
        assert_eq!(
            layers,
            [
                "Agent",
                "Changes",
                "Merged",
                "Push",
                "Pull requests",
                "Activity"
            ]
        );
        assert_eq!(shown[2].text, "yes: a squash merge into main");
        assert_eq!(shown[2].source, "wt");
        assert_eq!(shown[4].source, "GitHub");
        assert_eq!(shown[5].text, "2 days without activity (limit: 1 day)");
        assert_eq!(shown[5].mark, Mark::Pass);
    }

    #[test]
    fn the_layer_merged_tells_the_commits_and_why_it_is_unknown() {
        let not_merged = lines(&facts(), Reason::Old, 7);
        assert_eq!(not_merged[2].text, "no: 2 commits not in main");
        assert_eq!(not_merged[2].mark, Mark::Info);

        let empty = Facts {
            integration: Integration::Undetermined("the branch has no commit".to_owned()),
            ..facts()
        };
        let unknown = lines(&empty, Reason::Old, 7);
        assert_eq!(unknown[2].text, "unknown: the branch has no commit");
        assert_eq!(unknown[2].mark, Mark::Unknown);
    }

    #[test]
    fn a_merged_worktree_kept_today_shows_the_limit_of_1_day() {
        let kept = Facts {
            integration: Integration::Integrated("same_commit".to_owned()),
            days: Some(0),
            ..facts()
        };

        let activity = &lines(&kept, Reason::Recent, 7)[5];

        assert_eq!(activity.text, "0 days without activity (limit: 1 day)");
        assert_eq!(activity.mark, Mark::Fail);
        assert_eq!(
            reason_text(
                Reason::InDefault,
                &Facts {
                    pull_requests: PullRequests::InDefault(4),
                    ..facts()
                }
            ),
            "PR #4 finished, commit in the default branch"
        );
    }

    #[test]
    fn a_running_check_shows_its_layers_as_waiting() {
        let facts = Facts {
            changes: Changes::NotChecked,
            integration: Integration::NotChecked,
            push: Push::NotChecked,
            pull_requests: PullRequests::NotChecked,
            days: None,
            ..facts()
        };

        let marks: Vec<Mark> = lines(&facts, Reason::Checking, 7)
            .iter()
            .map(|line| line.mark)
            .collect();

        assert_eq!(
            marks,
            [
                Mark::Pass,
                Mark::Wait,
                Mark::Wait,
                Mark::Wait,
                Mark::Wait,
                Mark::Wait
            ]
        );
    }

    #[test]
    fn the_json_of_wt_gives_the_merge_of_each_worktree() {
        let json: Value =
            serde_json::from_str(include_str!("../tests/fixtures/wt-list.json")).unwrap();

        let integrations = parse_integrations(&json);

        assert_eq!(
            integrations["feat-squashed"],
            Integration::Integrated("patch_id_match".to_owned())
        );
        assert_eq!(
            integrations["feat-open"],
            Integration::NotIntegrated(Some(1))
        );
        assert_eq!(
            integrations["feat-orphan"],
            Integration::Undetermined("wt could not decide in time".to_owned())
        );
        assert_eq!(
            integrations["feat-empty"],
            Integration::Undetermined("the branch has no commit".to_owned())
        );
        assert_eq!(parse_default_branch(&json).as_deref(), Some("main"));
    }

    #[test]
    fn the_push_state_comes_from_the_upstream_and_the_remote_branches() {
        let branches = "feat-a\torigin/feat-a\t\nfeat-gone\torigin/feat-gone\t[gone]\nfeat-no-u\t\t\nfeat-local\t\t\n";
        let remote_branches = "origin/HEAD\norigin/feat-a\norigin/feat-no-u\n";

        let pushes = parse_pushes(branches, remote_branches);

        assert_eq!(pushes["feat-a"], Push::Pushed("origin/feat-a".to_owned()));
        assert_eq!(
            pushes["feat-gone"],
            Push::Gone("origin/feat-gone".to_owned())
        );
        assert_eq!(
            pushes["feat-no-u"],
            Push::Pushed("origin/feat-no-u".to_owned())
        );
        assert_eq!(pushes["feat-local"], Push::Never);
    }

    #[test]
    fn the_row_tells_the_proof_of_the_verdict() {
        let squashed = Facts {
            integration: Integration::Integrated("patch_id_match".to_owned()),
            ..facts()
        };
        let never = Facts {
            push: Push::Never,
            ..facts()
        };

        assert_eq!(
            reason_text(Reason::Integrated, &squashed),
            "merged: a squash merge into main"
        );
        assert_eq!(
            reason_text(Reason::Old, &never),
            "never pushed, no recent activity"
        );
        assert_eq!(
            reason_text(
                Reason::Unknown,
                &Facts {
                    pull_requests: PullRequests::Failed("no backend for GitLab yet".to_owned()),
                    ..facts()
                }
            ),
            "not checked: no backend for GitLab yet"
        );
    }
}
