//! The facts of a worktree, layer by layer, and the verdict of the
//! cleaner: to delete, or kept.
//!
//! The layers, from the cheapest to the most specific:
//!
//! 1. Agent (herdr): no agent, idle agents, or an agent that works.
//! 2. Changes (git status): changes that are not committed.
//! 3. Merged (wt): the content of the branch is in the default branch,
//!    for example after a merge or a squash merge. Any forge. The reflog
//!    of the branch tells if it has its own commits.
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
//! 4. The content is in the default branch (see `merged_branch`): to
//!    delete at once if the branch has its own commits, else after 1 day
//!    without activity and without agent.
//! 5. No pull request (the forge says so, or the branch was never pushed
//!    and the forge gives no answer), and no activity for `stale_days`
//!    days: to delete.
//! 6. Else kept: recent activity (also a merged branch with activity
//!    today), work after a finished pull request, a finished pull request
//!    with its commit in the default branch, or a forge that gave no
//!    answer.
//!
//! Idle agents (idle or done, in the worktree and in its workspace,
//! without the focus) do not keep a worktree with a proof of the end (rule
//! 3, and rule 4 with own commits): the deletion closes them first, as the
//! observer does. They keep a worktree without this proof: the age alone
//! does not prove that the work is finished.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::forge::Provider;
use crate::git;
use crate::herdr::Snapshot;
use crate::prune::{BranchStatus, KeptWhy};
use crate::style::{ACCENT, DIM, GREEN, RED, YELLOW};

/// The days without activity before a merged worktree without own commit
/// is a leftover: a new worktree has the commit of the default branch, so
/// it is merged at once. `wt step prune --min-age=1d` has the same limit.
pub const MERGED_MIN_DAYS: u64 = 1;

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
        resume_command(&self.kind, self.session.as_deref())
    }
}

/// The command that resumes the conversation of an agent of the kind
/// `kind` (claude, codex...). The cleaner and the observer write it after
/// they close an idle agent.
pub fn resume_command(kind: &str, session: Option<&str>) -> String {
    let session = session.unwrap_or("<session>");
    match kind {
        "claude" => format!("claude --resume {session}"),
        "codex" => format!("codex resume {session}"),
        kind => format!("{kind}, session {session}"),
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
    Clean,
    Uncommitted,
    /// git status failed.
    Failed,
}

/// The content of the branch in the default branch, from wt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Merge {
    #[default]
    NotChecked,
    /// The branch has commits that the default branch does not have: the
    /// number, if wt gives it.
    NotMerged(Option<u64>),
    /// The check of wt that found the content in the default branch, for
    /// example `patch_id_match` for a squash merge.
    Merged(String),
    /// wt gave no answer, for this reason.
    Undetermined(String),
}

/// The commits of the branch itself, from its reflog (see
/// `git::has_own_commits`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum History {
    #[default]
    NotChecked,
    /// A commit was made on the branch: its content in the default branch
    /// proves the end of its work.
    OwnCommits,
    /// No commit on the branch, or no reflog: a new or unused worktree.
    NoOwnCommit,
}

impl History {
    /// The history of a branch from the answer of git.
    pub fn of(has_own_commits: bool) -> History {
        if has_own_commits {
            History::OwnCommits
        } else {
            History::NoOwnCommit
        }
    }
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
    NoPullRequest,
    Open(u64),
    /// A finished pull request that contains the last commit.
    Finished {
        number: u64,
        merged: bool,
    },
    /// The pull requests are finished, but the worktree stays: the
    /// number of the last one, and why.
    Kept {
        number: u64,
        why: KeptWhy,
    },
    /// The forge gave no answer, for this reason.
    Failed(String),
}

impl PullRequests {
    /// The facts of a status of the forge.
    pub fn from_status(status: Result<BranchStatus, String>) -> PullRequests {
        match status {
            Err(error) => PullRequests::Failed(error),
            Ok(BranchStatus::NoPullRequest) => PullRequests::NoPullRequest,
            Ok(BranchStatus::Open(number)) => PullRequests::Open(number),
            Ok(BranchStatus::Finished { pull_request, .. }) => PullRequests::Finished {
                number: pull_request.number,
                merged: pull_request.state == crate::forge::PullRequestState::Merged,
            },
            Ok(BranchStatus::Kept { number, why }) => PullRequests::Kept { number, why },
        }
    }

    /// The number of the pull request, if the facts name one.
    pub fn number(&self) -> Option<u64> {
        match self {
            PullRequests::Open(number)
            | PullRequests::Finished { number, .. }
            | PullRequests::Kept { number, .. } => Some(*number),
            _ => None,
        }
    }
}

/// The facts of a worktree, layer by layer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub agent: Agents,
    pub changes: Changes,
    pub merge: Merge,
    /// Checked only for a merged branch.
    #[serde(default)]
    pub history: History,
    pub push: Push,
    pub pull_requests: PullRequests,
    /// The provider of the forge that gave the pull requests.
    #[serde(default)]
    pub forge: Option<Provider>,
    /// The name of the default branch of the repository (`main`), for the
    /// user.
    #[serde(default)]
    pub default_branch_name: Option<String>,
    /// The days since the last activity, once known.
    pub days: Option<u64>,
}

impl Facts {
    /// The name of the default branch, for the texts.
    fn default_branch_text(&self) -> &str {
        self.default_branch_name
            .as_deref()
            .unwrap_or("the default branch")
    }

    /// The provider of the pull requests, for their words. Without forge,
    /// the words of an unknown forge: `PR #3`.
    fn provider(&self) -> Provider {
        self.forge.unwrap_or(Provider::Unknown)
    }

    fn number_text(&self, number: u64) -> String {
        self.provider().number_text(number)
    }

    /// The number of the pull request of the facts, as the forge writes
    /// it, or an empty text.
    fn pull_request_number_text(&self) -> String {
        self.pull_requests
            .number()
            .map(|number| self.number_text(number))
            .unwrap_or_default()
    }

    fn short_name(&self) -> &'static str {
        self.provider().short_name()
    }

    fn long_name(&self) -> &'static str {
        self.provider().long_name()
    }
}

/// The verdict of the cleaner for a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// The checks run.
    Checking,
    /// To delete: a merged pull request contains the last commit.
    PullRequestMerged,
    /// To delete: a closed pull request contains the last commit.
    PullRequestClosed,
    /// To delete: the content is in the default branch.
    BranchMerged,
    /// To delete: no pull request, and no activity for `stale_days` days.
    Old,
    /// Kept: an open pull request.
    PullRequestOpen,
    /// Kept: no pull request, and some activity in the last `stale_days`
    /// days.
    Recent,
    /// Kept: the branch is merged without own commit, and its last
    /// activity is less than `MERGED_MIN_DAYS` old. A new worktree is
    /// merged at once.
    MergedRecently,
    /// Kept: changes that are not committed.
    Changes,
    /// Kept: an agent works in the worktree.
    Agent,
    /// Kept: an idle agent, and no proof that the work is finished.
    IdleAgent,
    /// Kept: the branch has work after its finished pull request.
    WorkAfterPullRequest,
    /// Kept: a finished pull request, and the default branch contains the
    /// commit, but wt did not see the merge.
    CommitInDefaultBranch,
    /// Kept: a check gave no answer.
    Unknown,
}

/// Return true if the deletion closes the idle agents of the worktree
/// first: a leftover, and only idle agents. A leftover with idle agents
/// always has a proof of the end: the verdict gives `IdleAgent` (kept), not
/// `Old`, to an old worktree with idle agents.
pub fn closes_idle_agents(reason: Reason, facts: &Facts) -> bool {
    reason.is_leftover() && facts.agent == Agents::Idle
}

impl Reason {
    /// Return true for a leftover: the group "To delete".
    pub fn is_leftover(self) -> bool {
        matches!(
            self,
            Reason::PullRequestMerged
                | Reason::PullRequestClosed
                | Reason::BranchMerged
                | Reason::Old
        )
    }
}

/// The rule for a branch whose content is in the default branch. The
/// cleaner and the observer apply it.
///
/// - Its own commits prove the end of its work: to delete at once. Idle
///   agents close first.
/// - Without its own commit, the worktree is new or unused: a new worktree
///   starts on the commit of the default branch. It is a leftover only
///   without an agent, after `MERGED_MIN_DAYS` days without activity.
///
/// Return `BranchMerged` (to delete), `Agent` or `IdleAgent` (kept), or
/// None if the activity is recent or unknown.
pub fn merged_branch(history: History, days: Option<u64>, agent: Agents) -> Option<Reason> {
    let is_old = days.is_some_and(|days| days >= MERGED_MIN_DAYS);
    match (agent, history) {
        (Agents::Busy, _) => Some(Reason::Agent),
        (_, History::OwnCommits) => Some(Reason::BranchMerged),
        _ if !is_old => None,
        (Agents::Idle, _) => Some(Reason::IdleAgent),
        (Agents::None, _) => Some(Reason::BranchMerged),
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

/// The verdict from the layers after an agent that works.
fn verdict_of_work(facts: &Facts, stale_days: u32) -> Reason {
    match facts.changes {
        Changes::Clean => {}
        Changes::Uncommitted => return Reason::Changes,
        Changes::Failed | Changes::NotChecked => return Reason::Unknown,
    }
    match facts.pull_requests {
        PullRequests::Open(_) => return Reason::PullRequestOpen,
        PullRequests::Finished { merged: true, .. } => return Reason::PullRequestMerged,
        PullRequests::Finished { merged: false, .. } => return Reason::PullRequestClosed,
        _ => {}
    }
    let is_merged = matches!(facts.merge, Merge::Merged(_));
    if is_merged {
        if let Some(reason) = merged_branch(facts.history, facts.days, facts.agent) {
            return reason;
        }
    }
    // A branch that was never pushed has no pull request, but only if the
    // forge gives no answer: a push without -u, or to another name, can
    // leave no remote branch here.
    let has_no_pull_request = matches!(facts.pull_requests, PullRequests::NoPullRequest)
        || (matches!(facts.push, Push::Never) && forge_is_silent(&facts.pull_requests));
    if has_no_pull_request {
        return match facts.days {
            Some(days) if days >= u64::from(stale_days) => Reason::Old,
            Some(_) if is_merged => Reason::MergedRecently,
            Some(_) => Reason::Recent,
            None => Reason::Unknown,
        };
    }
    match facts.pull_requests {
        PullRequests::Kept {
            why: KeptWhy::WorkAfter,
            ..
        } => Reason::WorkAfterPullRequest,
        PullRequests::Kept {
            why: KeptWhy::InDefaultBranch,
            ..
        } => Reason::CommitInDefaultBranch,
        _ if is_merged && facts.days.is_some() => Reason::MergedRecently,
        _ => Reason::Unknown,
    }
}

/// The days without activity that the verdict needs: none for a merged
/// branch with its own commits, `MERGED_MIN_DAYS` for another merged
/// branch, else `stale_days`.
pub fn activity_limit(facts: &Facts, stale_days: u32) -> Option<u64> {
    match (&facts.merge, facts.history) {
        (Merge::Merged(_), History::OwnCommits) => None,
        (Merge::Merged(_), _) => Some(MERGED_MIN_DAYS),
        _ => Some(u64::from(stale_days)),
    }
}

/// The rule of the verdict, on short lines: the preview of the cleaner
/// does not wrap.
pub fn rule_lines(stale_days: u32) -> Vec<String> {
    vec![
        "To delete, in this order:".to_owned(),
        "- no agent that works, and no change,".to_owned(),
        "- no open pull request,".to_owned(),
        "- a finished pull request with the last commit,".to_owned(),
        "- or a merged branch with its own commits,".to_owned(),
        "- or a merged branch without own commit,".to_owned(),
        format!(
            "  {} without activity and no agent,",
            days_text(MERGED_MIN_DAYS)
        ),
        format!(
            "- or no pull request, {} without activity and no agent.",
            days_text(u64::from(stale_days))
        ),
    ]
}

/// A number of days: `1 day`, `7 days`.
fn days_text(days: u64) -> String {
    if days == 1 {
        "1 day".to_owned()
    } else {
        format!("{days} days")
    }
}

/// The time of a change: `today`, `1 day ago`, `3 days ago`.
fn ago_text(days: u64) -> String {
    if days == 0 {
        "today".to_owned()
    } else {
        format!("{} ago", days_text(days))
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
fn merge_words(reason: &str, main: &str) -> String {
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

/// The detail of a check that gave no answer, for the row.
fn unknown_detail(facts: &Facts) -> String {
    match (&facts.changes, &facts.pull_requests) {
        (Changes::Failed, _) => "git status failed".to_owned(),
        (_, PullRequests::Failed(error)) => error.clone(),
        (
            _,
            PullRequests::Kept {
                why: KeptWhy::NoLocalCommit,
                ..
            },
        ) => "no local commit".to_owned(),
        _ if facts.days.is_none() => "no date of activity".to_owned(),
        _ => "no answer".to_owned(),
    }
}

/// The text of a verdict in a row of the list.
pub fn reason_text(reason: Reason, facts: &Facts) -> String {
    let number = facts.pull_request_number_text();
    let short = facts.short_name();
    let never_pushed = matches!(facts.push, Push::Never);
    let text = match reason {
        Reason::Checking => "checking…".to_owned(),
        Reason::PullRequestMerged => format!("{short} {number} merged"),
        Reason::PullRequestClosed => format!("{short} {number} closed"),
        Reason::BranchMerged => match &facts.merge {
            Merge::Merged(how) => {
                format!("merged: {}", merge_words(how, facts.default_branch_text()))
            }
            _ => format!("merged into {}", facts.default_branch_text()),
        },
        Reason::Old if never_pushed => "never pushed, no recent activity".to_owned(),
        Reason::Old => format!("no {short}, no recent activity"),
        Reason::PullRequestOpen => format!("{short} {number} open"),
        Reason::MergedRecently => "merged, recent activity".to_owned(),
        Reason::Recent if never_pushed => "never pushed, recent activity".to_owned(),
        Reason::Recent => format!("no {short}, recent activity"),
        Reason::Changes => "changes not committed".to_owned(),
        Reason::Agent => "an agent works here".to_owned(),
        Reason::IdleAgent => "an idle agent, no proof of the end".to_owned(),
        Reason::WorkAfterPullRequest => format!("work after {short} {number}"),
        Reason::CommitInDefaultBranch => {
            format!("{short} {number} finished, commit in the default branch")
        }
        Reason::Unknown => format!("not checked: {}", unknown_detail(facts)),
    };
    if closes_idle_agents(reason, facts) {
        format!("{text}, idle agent closed first")
    } else {
        text
    }
}

/// The text of a verdict in the question of the popup confirm.
pub fn question(reason: Reason, facts: &Facts) -> String {
    let number = facts.pull_request_number_text();
    let name = facts.long_name();
    let days = facts.days.unwrap_or(0);
    match reason {
        Reason::PullRequestMerged => format!("Its {name} {number} is merged."),
        Reason::PullRequestClosed => format!("Its {name} {number} is closed."),
        Reason::BranchMerged => format!("{} contains its content.", facts.default_branch_text()),
        Reason::Old => format!("No {name}, and no change for {}.", days_text(days)),
        Reason::PullRequestOpen => format!("Careful: its {name} {number} is open."),
        Reason::Recent => format!(
            "No proof that it is finished, and a change {}.",
            ago_text(days)
        ),
        Reason::MergedRecently => format!(
            "{} contains its content, but it had a change {}.",
            facts.default_branch_text(),
            ago_text(days)
        ),
        Reason::Changes => "Careful: it has changes that are not committed.".to_owned(),
        Reason::Agent => "Careful: an agent works in it.".to_owned(),
        Reason::IdleAgent => {
            "Careful: an idle agent is in it, and no proof shows the end of its work.".to_owned()
        }
        Reason::WorkAfterPullRequest => format!("Careful: the branch has work after {number}."),
        Reason::CommitInDefaultBranch => {
            format!("Its {name} {number} is finished, and the default branch contains its commit.")
        }
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

/// Return true if the forge gave no answer about the pull requests: then
/// only the push state of git can tell that a branch has no pull request.
fn forge_is_silent(pull_requests: &PullRequests) -> bool {
    matches!(
        pull_requests,
        PullRequests::Failed(_) | PullRequests::NotChecked | PullRequests::NotNeeded(_)
    )
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
    vec![
        agent_line(facts, reason),
        changes_line(facts, reason),
        merge_line(facts, reason),
        push_line(facts, reason),
        pull_requests_line(facts, reason),
        activity_line(facts, reason, stale_days),
    ]
}

/// The line of a layer without its fact: the check runs, an agent works
/// (the cleaner skips the checks), or the check gave no answer.
fn missing_line(facts: &Facts, reason: Reason, layer: &'static str, source: &str) -> Line {
    if reason == Reason::Checking {
        line(Mark::Wait, layer, "checking…", source)
    } else if facts.agent == Agents::Busy {
        line(Mark::Info, layer, "not needed", source)
    } else {
        line(Mark::Unknown, layer, "no answer", source)
    }
}

fn agent_line(facts: &Facts, reason: Reason) -> Line {
    let agent = |mark: Mark, text: &str| line(mark, "Agent", text, "herdr");
    match facts.agent {
        Agents::None => agent(Mark::Pass, "none"),
        Agents::Busy => agent(Mark::Fail, "an agent works here"),
        Agents::Idle if closes_idle_agents(reason, facts) => {
            agent(Mark::Pass, "idle: it closes before the deletion")
        }
        Agents::Idle if reason == Reason::IdleAgent => {
            agent(Mark::Fail, "idle, but no proof of the end of its work")
        }
        Agents::Idle => agent(Mark::Info, "idle"),
    }
}

fn changes_line(facts: &Facts, reason: Reason) -> Line {
    let changes = |mark: Mark, text: &str| line(mark, "Changes", text, "git status");
    match facts.changes {
        Changes::Clean => changes(Mark::Pass, "none"),
        Changes::Uncommitted => changes(Mark::Fail, "not committed"),
        Changes::Failed => changes(Mark::Unknown, "git status failed"),
        Changes::NotChecked => missing_line(facts, reason, "Changes", "git status"),
    }
}

fn merge_line(facts: &Facts, reason: Reason) -> Line {
    let main = facts.default_branch_text();
    let merge = |mark: Mark, text: String| line(mark, "Merged", text, "wt");
    match &facts.merge {
        Merge::Merged(how) => {
            let commits = match facts.history {
                History::OwnCommits => ", own commits",
                History::NoOwnCommit => ", no own commit",
                History::NotChecked => "",
            };
            merge(
                Mark::Pass,
                format!("yes: {}{commits}", merge_words(how, main)),
            )
        }
        Merge::NotMerged(Some(commits)) => merge(
            Mark::Info,
            format!("no: {} not in {main}", commits_text(*commits)),
        ),
        Merge::NotMerged(None) => merge(Mark::Info, format!("no: commits not in {main}")),
        Merge::Undetermined(why) => merge(Mark::Unknown, format!("unknown: {why}")),
        Merge::NotChecked => missing_line(facts, reason, "Merged", "wt"),
    }
}

fn push_line(facts: &Facts, reason: Reason) -> Line {
    let push = |mark: Mark, text: String| line(mark, "Push", text, "git");
    match &facts.push {
        Push::Never if forge_is_silent(&facts.pull_requests) => push(
            Mark::Pass,
            "never pushed: no pull request is possible".to_owned(),
        ),
        // A push without -u, or a branch from the ref of a pull request,
        // has no remote branch here: the forge decides.
        Push::Never => push(
            Mark::Info,
            "no remote branch here: the forge decides".to_owned(),
        ),
        Push::Pushed(remote) => push(Mark::Info, format!("pushed to {remote}")),
        Push::Gone(remote) => push(Mark::Info, format!("{remote} is deleted (a hint only)")),
        Push::NotChecked => missing_line(facts, reason, "Push", "git"),
    }
}

fn pull_requests_line(facts: &Facts, reason: Reason) -> Line {
    let forge = match facts.forge {
        Some(provider) => provider.name(),
        None if reason == Reason::Checking => "forge",
        None => "no forge",
    };
    let number = facts.pull_request_number_text();
    let pull_requests = |mark: Mark, text: String| line(mark, "Pull requests", text, forge);
    match &facts.pull_requests {
        PullRequests::NoPullRequest => pull_requests(Mark::Pass, "none".to_owned()),
        PullRequests::Finished { merged, .. } => pull_requests(
            Mark::Pass,
            format!(
                "{number} {}, with the last commit",
                if *merged { "merged" } else { "closed" }
            ),
        ),
        PullRequests::Open(_) => pull_requests(Mark::Fail, format!("{number} open")),
        PullRequests::Kept { why, .. } => {
            let (mark, words) = match why {
                KeptWhy::WorkAfter => (Mark::Fail, "but the branch has commits after it"),
                KeptWhy::InDefaultBranch => (Mark::Info, "the default branch has the commit"),
                KeptWhy::NoLocalCommit => (Mark::Unknown, "but no local commit"),
            };
            pull_requests(mark, format!("{number} finished, {words}"))
        }
        PullRequests::NotNeeded(why) => pull_requests(Mark::Info, format!("not needed: {why}")),
        PullRequests::Failed(error) => pull_requests(Mark::Unknown, format!("no answer: {error}")),
        PullRequests::NotChecked => missing_line(facts, reason, "Pull requests", forge),
    }
}

fn activity_line(facts: &Facts, reason: Reason, stale_days: u32) -> Line {
    let Some(days) = facts.days else {
        return missing_line(facts, reason, "Activity", "git");
    };
    let Some(limit) = activity_limit(facts, stale_days) else {
        return line(
            Mark::Info,
            "Activity",
            format!("{} without activity (no limit)", days_text(days)),
            "git",
        );
    };
    let mark = if days >= limit {
        Mark::Pass
    } else if matches!(reason, Reason::Recent | Reason::MergedRecently) {
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

/// The name of the default branch, from the JSON of `wt list`.
pub fn parse_default_branch(json: &Value) -> Option<String> {
    json["repo"]["default_branch"].as_str().map(str::to_owned)
}

/// The merge of each worktree in the default branch, from the JSON of
/// `wt list --format json`: `default_branch.integration` is absent when wt
/// found no merge, null when wt could not decide, and an object with the
/// reason when the default branch has the content.
pub fn parse_merges(json: &Value) -> HashMap<String, Merge> {
    let mut merges = HashMap::new();
    for item in json["items"].as_array().into_iter().flatten() {
        let Some(branch) = item["branch"].as_str() else {
            continue;
        };
        let default_branch = &item["default_branch"];
        let merge = if item.get("head").is_some_and(Value::is_null) {
            Merge::Undetermined("the branch has no commit".to_owned())
        } else if !default_branch.is_object() {
            Merge::Undetermined("wt gave no answer".to_owned())
        } else if default_branch["orphan"].as_bool() == Some(true) {
            Merge::Undetermined("no common history with the default branch".to_owned())
        } else {
            match default_branch.get("integration") {
                None => Merge::NotMerged(default_branch["ahead"].as_u64()),
                Some(Value::Null) => Merge::Undetermined("wt could not decide in time".to_owned()),
                Some(found) => {
                    Merge::Merged(found["reason"].as_str().unwrap_or("integrated").to_owned())
                }
            }
        };
        merges.insert(branch.to_owned(), merge);
    }
    merges
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
            changes: Changes::Clean,
            merge: Merge::NotMerged(Some(2)),
            history: History::NotChecked,
            push: Push::Pushed("origin/feat".to_owned()),
            pull_requests: PullRequests::NoPullRequest,
            forge: Some(Provider::GitHub),
            default_branch_name: Some("main".to_owned()),
            days: Some(30),
        }
    }

    /// The line of a layer, by its name.
    fn layer<'a>(shown: &'a [Line], name: &str) -> &'a Line {
        shown
            .iter()
            .find(|line| line.layer == name)
            .unwrap_or_else(|| panic!("no layer {name}"))
    }

    #[test]
    fn a_layer_without_answer_after_the_checks_is_unknown() {
        let facts = Facts {
            push: Push::NotChecked,
            days: None,
            ..facts()
        };

        let after_checks = lines(&facts, Reason::Unknown, 7);
        let busy = Facts {
            agent: Agents::Busy,
            ..facts.clone()
        };
        let busy_agent = lines(&busy, Reason::Agent, 7);

        for name in ["Push", "Activity"] {
            assert_eq!(layer(&after_checks, name).mark, Mark::Unknown, "{name}");
            assert_eq!(layer(&after_checks, name).text, "no answer", "{name}");
            // An agent that works needs no check.
            assert_eq!(layer(&busy_agent, name).mark, Mark::Info, "{name}");
            assert_eq!(layer(&busy_agent, name).text, "not needed", "{name}");
        }
    }

    #[test]
    fn the_texts_count_the_days() {
        let merged_today = Facts {
            merge: Merge::Merged("same_commit".to_owned()),
            pull_requests: PullRequests::NoPullRequest,
            days: Some(0),
            ..facts()
        };

        assert_eq!(
            question(
                Reason::Old,
                &Facts {
                    pull_requests: PullRequests::NoPullRequest,
                    days: Some(1),
                    ..facts()
                }
            ),
            "No pull request, and no change for 1 day."
        );
        assert_eq!(
            question(
                Reason::Recent,
                &Facts {
                    days: Some(0),
                    ..facts()
                }
            ),
            "No proof that it is finished, and a change today."
        );
        assert_eq!(
            question(Reason::MergedRecently, &merged_today),
            "main contains its content, but it had a change today."
        );
        assert_eq!(
            reason_text(Reason::MergedRecently, &merged_today),
            "merged, recent activity"
        );
        assert_eq!(verdict(&merged_today, 7), Reason::MergedRecently);
        assert_eq!(
            rule_lines(7)[4..],
            [
                "- or a merged branch with its own commits,",
                "- or a merged branch without own commit,",
                "  1 day without activity and no agent,",
                "- or no pull request, 7 days without activity and no agent.",
            ]
        );
    }

    #[test]
    fn the_verdict_covers_the_rare_cases_of_the_forge() {
        // A finished pull request, but the branch has no local commit.
        let no_local_commit = Facts {
            pull_requests: PullRequests::Kept {
                number: 5,
                why: KeptWhy::NoLocalCommit,
            },
            ..facts()
        };
        // Rule 4 comes before rule 6: a merged branch is a leftover, also
        // with work after a finished pull request.
        let merged_with_work_after = Facts {
            merge: Merge::Merged("patch_id_match".to_owned()),
            pull_requests: PullRequests::Kept {
                number: 5,
                why: KeptWhy::WorkAfter,
            },
            days: Some(1),
            ..facts()
        };

        assert_eq!(verdict(&no_local_commit, 7), Reason::Unknown);
        assert_eq!(
            reason_text(Reason::Unknown, &no_local_commit),
            "not checked: no local commit"
        );
        assert_eq!(verdict(&merged_with_work_after, 7), Reason::BranchMerged);
    }

    #[test]
    fn the_verdict_follows_the_order_of_the_layers() {
        let squash = Merge::Merged("patch_id_match".to_owned());
        let merged_pr = PullRequests::Finished {
            number: 3,
            merged: true,
        };
        let no_backend = PullRequests::Failed("no backend for an unknown forge yet".to_owned());
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
                    changes: Changes::Uncommitted,
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
                    merge: squash.clone(),
                    ..facts()
                },
                Reason::PullRequestOpen,
            ),
            (
                "merged PR",
                Facts {
                    pull_requests: merged_pr.clone(),
                    days: Some(0),
                    ..facts()
                },
                Reason::PullRequestMerged,
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
                Reason::PullRequestClosed,
            ),
            (
                "merged, 1 day without activity, the forge failed",
                Facts {
                    merge: squash.clone(),
                    pull_requests: PullRequests::Failed("gh: no end".to_owned()),
                    days: Some(1),
                    ..facts()
                },
                Reason::BranchMerged,
            ),
            (
                "a new worktree is merged today: kept",
                Facts {
                    merge: Merge::Merged("same_commit".to_owned()),
                    pull_requests: PullRequests::NotNeeded("no remote origin".to_owned()),
                    push: Push::Never,
                    days: Some(0),
                    ..facts()
                },
                Reason::MergedRecently,
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
                Reason::PullRequestOpen,
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
                    merge: Merge::Undetermined("wt gave no answer".to_owned()),
                    pull_requests: PullRequests::Kept {
                        number: 4,
                        why: KeptWhy::InDefaultBranch,
                    },
                    ..facts()
                },
                Reason::CommitInDefaultBranch,
            ),
            (
                "work after a finished PR",
                Facts {
                    pull_requests: PullRequests::Kept {
                        number: 3,
                        why: KeptWhy::WorkAfter,
                    },
                    ..facts()
                },
                Reason::WorkAfterPullRequest,
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
    fn only_a_leftover_closes_its_idle_agents() {
        let idle = Facts {
            agent: Agents::Idle,
            ..facts()
        };
        let busy = Facts {
            agent: Agents::Busy,
            ..facts()
        };

        assert!(closes_idle_agents(Reason::PullRequestMerged, &idle));
        assert!(closes_idle_agents(Reason::BranchMerged, &idle));
        assert!(!closes_idle_agents(Reason::IdleAgent, &idle));
        assert!(!closes_idle_agents(Reason::PullRequestOpen, &idle));
        assert!(!closes_idle_agents(Reason::PullRequestMerged, &busy));
        assert!(!closes_idle_agents(Reason::PullRequestMerged, &facts()));
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
        assert_eq!(verdict(&merged_pr, 7), Reason::PullRequestMerged);
        assert_eq!(
            reason_text(Reason::PullRequestMerged, &merged_pr),
            "PR #3 merged, idle agent closed first"
        );
        let squashed = idle(Facts {
            merge: Merge::Merged("patch_id_match".to_owned()),
            history: History::OwnCommits,
            days: Some(0),
            ..facts()
        });
        assert_eq!(verdict(&squashed, 7), Reason::BranchMerged);
        assert!(closes_idle_agents(Reason::BranchMerged, &squashed));
        // The age alone: the idle agent keeps the worktree.
        assert_eq!(verdict(&idle(facts()), 7), Reason::IdleAgent);
        // An open PR stays an open PR.
        let open = idle(Facts {
            pull_requests: PullRequests::Open(1),
            ..facts()
        });
        assert_eq!(verdict(&open, 7), Reason::PullRequestOpen);
    }

    #[test]
    fn a_merged_branch_goes_at_once_only_with_its_own_commits() {
        use History::{NoOwnCommit, NotChecked, OwnCommits};
        let cases = [
            // Own commits: the proof of the end, also today, also with an
            // idle agent.
            (
                OwnCommits,
                Some(0),
                Agents::None,
                Some(Reason::BranchMerged),
            ),
            (
                OwnCommits,
                Some(0),
                Agents::Idle,
                Some(Reason::BranchMerged),
            ),
            (OwnCommits, None, Agents::Idle, Some(Reason::BranchMerged)),
            // A new or unused worktree: only without agent, after 1 day.
            (
                NoOwnCommit,
                Some(1),
                Agents::None,
                Some(Reason::BranchMerged),
            ),
            (
                NotChecked,
                Some(1),
                Agents::None,
                Some(Reason::BranchMerged),
            ),
            (NoOwnCommit, Some(0), Agents::None, None),
            (NoOwnCommit, None, Agents::None, None),
            (NoOwnCommit, Some(9), Agents::Idle, Some(Reason::IdleAgent)),
            (NoOwnCommit, Some(0), Agents::Idle, None),
            // An agent that works always keeps the worktree.
            (OwnCommits, Some(9), Agents::Busy, Some(Reason::Agent)),
            (NoOwnCommit, Some(9), Agents::Busy, Some(Reason::Agent)),
        ];
        for (history, days, agent, expected) in cases {
            assert_eq!(
                merged_branch(history, days, agent),
                expected,
                "{history:?} {days:?} {agent:?}"
            );
        }
    }

    #[test]
    fn the_cleaner_applies_the_rule_of_a_merged_branch() {
        let merged = |history: History, days: u64, agent: Agents| Facts {
            merge: Merge::Merged("ancestor".to_owned()),
            history,
            days: Some(days),
            agent,
            ..facts()
        };

        // A merge without pull request (wt merge --no-remove), today.
        let local_merge = merged(History::OwnCommits, 0, Agents::Idle);
        assert_eq!(verdict(&local_merge, 7), Reason::BranchMerged);
        assert_eq!(
            reason_text(Reason::BranchMerged, &local_merge),
            "merged: an older commit of main, idle agent closed first"
        );
        let shown = lines(&local_merge, Reason::BranchMerged, 7);
        assert_eq!(
            layer(&shown, "Merged").text,
            "yes: an older commit of main, own commits"
        );
        assert_eq!(layer(&shown, "Activity").mark, Mark::Info);
        assert_eq!(
            layer(&shown, "Activity").text,
            "0 days without activity (no limit)"
        );
        // A fork that waits on the commit of main: kept.
        let waiting_fork = merged(History::NoOwnCommit, 3, Agents::Idle);
        assert_eq!(verdict(&waiting_fork, 7), Reason::IdleAgent);
        assert!(!closes_idle_agents(Reason::IdleAgent, &waiting_fork));
        let shown = lines(&waiting_fork, Reason::IdleAgent, 7);
        assert_eq!(
            layer(&shown, "Merged").text,
            "yes: an older commit of main, no own commit"
        );
        // An unused worktree without agent: as wt step prune --min-age=1d.
        assert_eq!(
            verdict(&merged(History::NoOwnCommit, 1, Agents::None), 7),
            Reason::BranchMerged
        );
        assert_eq!(
            verdict(&merged(History::NoOwnCommit, 0, Agents::None), 7),
            Reason::MergedRecently
        );
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
            merge: Merge::Merged("patch_id_match".to_owned()),
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
    fn gitlab_gets_the_words_of_its_merge_requests() {
        let gitlab = |pull_requests| Facts {
            pull_requests,
            forge: Some(Provider::GitLab),
            ..facts()
        };
        let merged = gitlab(PullRequests::Finished {
            number: 12,
            merged: true,
        });

        assert_eq!(
            reason_text(Reason::PullRequestMerged, &merged),
            "MR !12 merged"
        );
        assert_eq!(
            question(Reason::PullRequestMerged, &merged),
            "Its merge request !12 is merged."
        );
        assert_eq!(
            layer(
                &lines(&merged, Reason::PullRequestMerged, 7),
                "Pull requests"
            )
            .text,
            "!12 merged, with the last commit"
        );
        assert_eq!(
            reason_text(Reason::Old, &gitlab(PullRequests::NoPullRequest)),
            "no MR, no recent activity"
        );
        // GitHub keeps its words.
        let github = Facts {
            forge: Some(Provider::GitHub),
            ..merged
        };
        assert_eq!(
            reason_text(Reason::PullRequestMerged, &github),
            "PR #12 merged"
        );
        // Azure DevOps has pull requests, with the mark of GitLab.
        let azure = Facts {
            forge: Some(Provider::AzureDevOps),
            ..github
        };
        assert_eq!(
            reason_text(Reason::PullRequestMerged, &azure),
            "PR !12 merged"
        );
    }

    #[test]
    fn the_push_layer_does_not_deny_a_pull_request_of_the_forge() {
        let never_pushed = |pull_requests| Facts {
            push: Push::Never,
            pull_requests,
            ..facts()
        };
        let push_line = |facts: &Facts| {
            let shown = lines(facts, Reason::Recent, 7);
            let line = layer(&shown, "Push");
            (line.mark, line.text.clone())
        };

        assert_eq!(
            push_line(&never_pushed(PullRequests::Failed("gh failed".to_owned()))),
            (
                Mark::Pass,
                "never pushed: no pull request is possible".to_owned()
            )
        );
        assert_eq!(
            push_line(&never_pushed(PullRequests::Finished {
                number: 4,
                merged: true
            })),
            (
                Mark::Info,
                "no remote branch here: the forge decides".to_owned()
            )
        );
    }

    #[test]
    fn the_tab_checks_has_one_line_for_each_layer() {
        let facts = Facts {
            merge: Merge::Merged("patch_id_match".to_owned()),
            days: Some(2),
            ..facts()
        };

        let shown = lines(&facts, Reason::BranchMerged, 7);

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
        assert_eq!(
            layer(&shown, "Merged").text,
            "yes: a squash merge into main"
        );
        assert_eq!(layer(&shown, "Merged").source, "wt");
        assert_eq!(layer(&shown, "Pull requests").source, "GitHub");
        assert_eq!(
            layer(&shown, "Activity").text,
            "2 days without activity (limit: 1 day)"
        );
        assert_eq!(layer(&shown, "Activity").mark, Mark::Pass);
    }

    #[test]
    fn the_layer_merged_tells_the_commits_and_why_it_is_unknown() {
        let not_merged = lines(&facts(), Reason::Old, 7);
        assert_eq!(
            layer(&not_merged, "Merged").text,
            "no: 2 commits not in main"
        );
        assert_eq!(layer(&not_merged, "Merged").mark, Mark::Info);

        let empty = Facts {
            merge: Merge::Undetermined("the branch has no commit".to_owned()),
            ..facts()
        };
        let unknown = lines(&empty, Reason::Old, 7);
        assert_eq!(
            layer(&unknown, "Merged").text,
            "unknown: the branch has no commit"
        );
        assert_eq!(layer(&unknown, "Merged").mark, Mark::Unknown);
    }

    #[test]
    fn a_merged_worktree_kept_today_shows_the_limit_of_1_day() {
        let kept = Facts {
            merge: Merge::Merged("same_commit".to_owned()),
            days: Some(0),
            ..facts()
        };

        let shown = lines(&kept, Reason::MergedRecently, 7);
        let activity = layer(&shown, "Activity");

        assert_eq!(activity.text, "0 days without activity (limit: 1 day)");
        assert_eq!(activity.mark, Mark::Fail);
        assert_eq!(
            reason_text(
                Reason::CommitInDefaultBranch,
                &Facts {
                    pull_requests: PullRequests::Kept {
                        number: 4,
                        why: KeptWhy::InDefaultBranch,
                    },
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
            merge: Merge::NotChecked,
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

        let merges = parse_merges(&json);

        assert_eq!(
            merges["feat-squashed"],
            Merge::Merged("patch_id_match".to_owned())
        );
        assert_eq!(merges["feat-open"], Merge::NotMerged(Some(1)));
        assert_eq!(
            merges["feat-orphan"],
            Merge::Undetermined("wt could not decide in time".to_owned())
        );
        assert_eq!(
            merges["feat-empty"],
            Merge::Undetermined("the branch has no commit".to_owned())
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
            merge: Merge::Merged("patch_id_match".to_owned()),
            ..facts()
        };
        let never = Facts {
            push: Push::Never,
            ..facts()
        };

        assert_eq!(
            reason_text(Reason::BranchMerged, &squashed),
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
                    pull_requests: PullRequests::Failed(
                        "no backend for an unknown forge yet".to_owned()
                    ),
                    ..facts()
                }
            ),
            "not checked: no backend for an unknown forge yet"
        );
    }
}
