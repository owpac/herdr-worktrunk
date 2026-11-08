//! The observer keeps the rule "1 git worktree = 1 herdr workspace".
//!
//! herdr starts the observer with the plugin (`[[startup]]`). Every 2
//! seconds, the observer reads the snapshot of herdr and does 2 things:
//!
//! 1. If an agent works in another checkout of the same repository than
//!    its workspace (for example, Claude Code entered a worktree), the
//!    observer moves the pane of the agent to the workspace of this
//!    checkout. herdr opens the workspace if necessary.
//! 2. If the worktree of a workspace is removed (`wt remove`, `wt merge`)
//!    and no agent works in the workspace, the observer closes it.
//!
//! The observer watches only the agents. A shell that changes its folder
//! does not move. A change to another repository does not move.
//!
//! With `prune_merged = true`, a second thread removes the worktrees of
//! the merged pull requests every 5 minutes (see `prune`). It skips a
//! worktree where an agent works. Then the rule 2 closes the workspace.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::actions;
use crate::git::{self, Checkout, LinkedWorktree};
use crate::github::PullRequestState;
use crate::herdr::{self, Error, Pane, Snapshot};
use crate::prune::{self, FinishedWorktree};
use crate::stale;
use crate::wt;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The thread of the merged pull requests waits this time after the
/// start: the first snapshots give it the repositories.
const PRUNE_FIRST_DELAY: Duration = Duration::from_secs(30);
/// The time between 2 checks of the merged pull requests.
const PRUNE_INTERVAL: Duration = Duration::from_secs(300);
/// The folder of a pane rarely changes: ask git again after this time.
const CHECKOUT_CACHE_TIME: Duration = Duration::from_secs(30);
/// After this number of failed snapshots in a row, herdr is stopped.
const MAX_SNAPSHOT_FAILURES: u32 = 5;
const MAX_LOG_SIZE_BYTES: u64 = 1_000_000;

/// An agent pane, as the rules of the observer see it.
#[derive(Clone, Debug)]
pub struct AgentPane {
    pub pane_id: String,
    pub workspace_id: String,
    /// The checkout of the folder of the agent, if the folder is in git.
    pub checkout: Option<Checkout>,
    pub is_focused: bool,
    /// The kind of the agent: claude, codex...
    pub kind: String,
    /// The state of the agent: idle, working, blocked, done or unknown.
    pub status: Option<String>,
    /// The session ID of the agent, to resume its conversation.
    pub session: Option<String>,
}

/// A workspace, with the checkout that it showed when the observer first
/// saw it. herdr can change the checkout of a workspace later, when the
/// folder of its pane changes: the first checkout is the true one.
#[derive(Clone, Debug)]
pub struct KnownWorkspace {
    pub workspace_id: String,
    pub checkout_root: Option<PathBuf>,
    pub common_dir: Option<PathBuf>,
    pub is_linked_worktree: bool,
    pub has_agent: bool,
    pub checkout_exists: bool,
}

/// One change that the observer makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    MovePane {
        pane_id: String,
        checkout: Checkout,
        focus: bool,
    },
    CloseWorkspace {
        workspace_id: String,
    },
}

/// Decide the changes for one snapshot.
///
/// A pane moves only if the observer saw the same difference in the
/// previous snapshot too: a short change of folder does not move a pane.
///
/// Returns the changes, and the panes with a difference in this
/// snapshot (give them to the next call).
pub fn plan(
    agent_panes: &[AgentPane],
    workspaces: &[KnownWorkspace],
    previous_differences: &HashSet<String>,
) -> (Vec<Step>, HashSet<String>) {
    let mut steps = Vec::new();
    let mut differences = HashSet::new();
    for pane in agent_panes {
        let Some(checkout) = &pane.checkout else {
            continue;
        };
        let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == pane.workspace_id)
        else {
            continue;
        };
        let is_other_checkout = workspace.checkout_root.as_ref() != Some(&checkout.root);
        let is_same_repository = workspace.common_dir.as_ref() == Some(&checkout.common_dir);
        if !is_other_checkout || !is_same_repository {
            continue;
        }
        differences.insert(pane.pane_id.clone());
        if previous_differences.contains(&pane.pane_id) {
            steps.push(Step::MovePane {
                pane_id: pane.pane_id.clone(),
                checkout: checkout.clone(),
                focus: pane.is_focused,
            });
        }
    }
    for workspace in workspaces {
        let is_removed_worktree = workspace.is_linked_worktree && !workspace.checkout_exists;
        if is_removed_worktree && !workspace.has_agent {
            steps.push(Step::CloseWorkspace {
                workspace_id: workspace.workspace_id.clone(),
            });
        }
    }
    (steps, differences)
}

/// The checkout of a workspace when the observer first saw it.
#[derive(Clone, Debug)]
struct FirstCheckout {
    root: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    is_linked_worktree: bool,
}

/// The state that the observer keeps between 2 snapshots.
struct Observer {
    log: Arc<Log>,
    workspaces: HashMap<String, FirstCheckout>,
    folders: HashMap<PathBuf, (Instant, Option<Checkout>)>,
    differences: HashSet<String>,
    /// The view of the thread of the merged pull requests, if
    /// `prune_merged` is true.
    prune_view: Option<Arc<Mutex<PruneView>>>,
    /// The time when the observer first saw each workspace that appeared
    /// after its start.
    first_seen: HashMap<String, Instant>,
    /// The observer read its first snapshot.
    is_started: bool,
}

/// The age under which a new workspace can give its shell to an agent.
const FRESH_WORKSPACE_TIME: Duration = Duration::from_secs(120);

/// A shell pane that the pane of an agent replaces.
#[derive(Debug, PartialEq, Eq)]
struct FreshShell {
    pane_id: String,
    tab_id: String,
}

/// Return the only pane of a new workspace, if this pane is a shell
/// without agent.
///
/// The hook post-start of wt opens the workspace of a new worktree, with
/// a shell. A few seconds later, the agent that entered the worktree
/// comes: its pane takes the place of this shell. The workspace must be
/// younger than 2 minutes: the observer does not close a shell that the
/// user can use.
fn fresh_shell(age: Option<Duration>, workspace_id: &str, panes: &[Pane]) -> Option<FreshShell> {
    if !age.is_some_and(|age| age < FRESH_WORKSPACE_TIME) {
        return None;
    }
    let mut panes = panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace_id);
    let only = panes.next()?;
    if panes.next().is_some() || only.agent.is_some() {
        return None;
    }
    Some(FreshShell {
        pane_id: only.pane_id.clone(),
        tab_id: only.tab_id.clone(),
    })
}

/// What the thread of the merged pull requests reads from the observer.
/// The observer writes it at each snapshot. The paths are not canonical:
/// the thread makes them canonical, so the loop of the snapshots stays
/// cheap.
#[derive(Debug, Default)]
struct PruneView {
    /// The main checkout of each repository that a workspace shows.
    repositories: HashSet<PathBuf>,
    /// The agents, with their checkouts.
    agents: Vec<ViewAgent>,
}

/// An agent, as the thread of the merged pull requests sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ViewAgent {
    pane_id: String,
    kind: String,
    status: Option<String>,
    session: Option<String>,
    /// The checkout of the folder of the agent.
    folder_root: Option<PathBuf>,
    /// The checkout of the workspace of the agent. The actions remove and
    /// merge refuse the same workspaces.
    workspace_root: Option<PathBuf>,
    /// The workspace of the agent has the focus.
    is_workspace_focused: bool,
}

impl ViewAgent {
    /// Return true if the folder of the agent is in `checkout`.
    fn works_in(&self, checkout: &Path) -> bool {
        self.folder_root
            .as_deref()
            .is_some_and(|root| canonical(root) == canonical(checkout))
    }

    /// Return true if the folder or the workspace of the agent is in
    /// `checkout`.
    fn uses(&self, checkout: &Path) -> bool {
        self.works_in(checkout)
            || self
                .workspace_root
                .as_deref()
                .is_some_and(|root| canonical(root) == canonical(checkout))
    }
}

impl PruneView {
    /// The agents whose folder or workspace is in `checkout`.
    fn agents_in(&self, checkout: &Path) -> Vec<ViewAgent> {
        self.agents
            .iter()
            .filter(|agent| agent.uses(checkout))
            .cloned()
            .collect()
    }
}

/// What the thread does with the agents of a finished worktree.
#[derive(Debug, PartialEq, Eq)]
enum AgentDecision {
    /// No agent uses the worktree: remove it.
    Remove,
    /// All the agents are idle or done, and work in the folder of the
    /// worktree: close their panes, then remove the worktree. The
    /// conversations stay on the disk.
    CloseThenRemove,
    /// An agent works, waits for an answer, has the focus, or works in
    /// another folder: keep the worktree until the next pass.
    Wait,
}

/// Decide what to do with the agents of the finished worktree `checkout`.
///
/// An agent whose workspace shows the worktree, but whose folder is in
/// another checkout, does not work for this worktree: the thread does not
/// close it, and keeps the worktree.
fn agent_decision(agents: &[ViewAgent], checkout: &Path) -> AgentDecision {
    if agents.is_empty() {
        return AgentDecision::Remove;
    }
    let may_close = |agent: &ViewAgent| {
        matches!(agent.status.as_deref(), Some("idle" | "done"))
            && !agent.is_workspace_focused
            && agent.works_in(checkout)
    };
    if agents.iter().all(may_close) {
        AgentDecision::CloseThenRemove
    } else {
        AgentDecision::Wait
    }
}

/// The command that resumes the conversation of an agent.
fn resume_command(agent: &ViewAgent) -> String {
    let session = agent.session.as_deref().unwrap_or("<session>");
    match agent.kind.as_str() {
        "claude" => format!("claude --resume {session}"),
        "codex" => format!("codex resume {session}"),
        kind => format!("{kind}, session {session}"),
    }
}

/// Lock the view. A panic of the other thread does not stop this thread.
fn lock(view: &Mutex<PruneView>) -> MutexGuard<'_, PruneView> {
    view.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The canonical path of a folder, or the path itself if the folder does
/// not exist. git and herdr can give 2 forms of the same folder, for
/// example `/tmp` and `/private/tmp` on macOS.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The agents for the view: each agent with the checkout of its folder
/// and the checkout of its workspace.
fn view_agents(
    agent_panes: &[AgentPane],
    workspaces: &HashMap<String, FirstCheckout>,
    focused_workspaces: &HashSet<String>,
) -> Vec<ViewAgent> {
    agent_panes
        .iter()
        .map(|pane| ViewAgent {
            pane_id: pane.pane_id.clone(),
            kind: pane.kind.clone(),
            status: pane.status.clone(),
            session: pane.session.clone(),
            folder_root: pane.checkout.as_ref().map(|checkout| checkout.root.clone()),
            workspace_root: workspaces
                .get(&pane.workspace_id)
                .and_then(|first| first.root.clone()),
            is_workspace_focused: focused_workspaces.contains(&pane.workspace_id),
        })
        .collect()
}

/// The main checkout of each repository that a workspace shows.
fn repositories(workspaces: &HashMap<String, FirstCheckout>) -> HashSet<PathBuf> {
    workspaces
        .values()
        .filter_map(|first| first.common_dir.as_deref().and_then(Path::parent))
        .map(Path::to_path_buf)
        .collect()
}

impl Observer {
    /// Return the checkout of a folder, from the cache when it is recent.
    fn checkout_of_folder(&mut self, folder: &Path) -> Option<Checkout> {
        if let Some((time, checkout)) = self.folders.get(folder) {
            if time.elapsed() < CHECKOUT_CACHE_TIME {
                return checkout.clone();
            }
        }
        let checkout = git::checkout_of(folder);
        self.folders
            .insert(folder.to_path_buf(), (Instant::now(), checkout.clone()));
        checkout
    }

    /// Remember the checkout of each new workspace, and forget the
    /// closed workspaces.
    fn update_workspaces(&mut self, snapshot: &Snapshot) {
        let live: HashSet<&str> = snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_id.as_str())
            .collect();
        self.workspaces.retain(|id, _| live.contains(id.as_str()));
        self.first_seen.retain(|id, _| live.contains(id.as_str()));
        for workspace in &snapshot.workspaces {
            if self.workspaces.contains_key(&workspace.workspace_id) {
                continue;
            }
            // The workspaces of the first snapshot are not new: the user
            // can use their shells.
            if self.is_started {
                self.first_seen
                    .insert(workspace.workspace_id.clone(), Instant::now());
            }
            let first = match herdr::workspace_checkout(&workspace.workspace_id) {
                Ok(Some(checkout)) => FirstCheckout {
                    common_dir: git::checkout_of(&checkout.checkout_path)
                        .map(|checkout| checkout.common_dir),
                    root: Some(checkout.checkout_path),
                    is_linked_worktree: checkout.is_linked_worktree,
                },
                Ok(None) => FirstCheckout {
                    root: None,
                    common_dir: None,
                    is_linked_worktree: false,
                },
                Err(error) => {
                    self.log.write(&format!(
                        "workspace get {}: {error}",
                        workspace.workspace_id
                    ));
                    continue;
                }
            };
            self.workspaces
                .insert(workspace.workspace_id.clone(), first);
        }
        self.is_started = true;
    }

    /// Read one snapshot, then make its changes.
    fn tick(&mut self, snapshot: &Snapshot) {
        self.update_workspaces(snapshot);
        let mut agent_panes = Vec::new();
        for pane in snapshot.panes.iter().filter(|pane| pane.agent.is_some()) {
            let checkout = pane
                .folder()
                .and_then(|folder| self.checkout_of_folder(folder));
            agent_panes.push(AgentPane {
                pane_id: pane.pane_id.clone(),
                workspace_id: pane.workspace_id.clone(),
                checkout,
                is_focused: pane.focused,
                kind: pane.agent.clone().unwrap_or_default(),
                status: pane.agent_status.clone(),
                session: pane
                    .agent_session
                    .as_ref()
                    .map(|session| session.value.clone()),
            });
        }
        let known: Vec<KnownWorkspace> = self
            .workspaces
            .iter()
            .map(|(workspace_id, first)| KnownWorkspace {
                workspace_id: workspace_id.clone(),
                checkout_root: first.root.clone(),
                common_dir: first.common_dir.clone(),
                is_linked_worktree: first.is_linked_worktree,
                has_agent: agent_panes
                    .iter()
                    .any(|pane| &pane.workspace_id == workspace_id),
                checkout_exists: first.root.as_deref().is_some_and(Path::exists),
            })
            .collect();
        if let Some(view) = &self.prune_view {
            let focused_workspaces: HashSet<String> = snapshot
                .workspaces
                .iter()
                .filter(|workspace| workspace.focused)
                .map(|workspace| workspace.workspace_id.clone())
                .collect();
            let new_view = PruneView {
                repositories: repositories(&self.workspaces),
                agents: view_agents(&agent_panes, &self.workspaces, &focused_workspaces),
            };
            *lock(view) = new_view;
        }
        let (steps, differences) = plan(&agent_panes, &known, &self.differences);
        self.differences = differences;
        for step in steps {
            if let Err(error) = self.apply(&step, &agent_panes, snapshot) {
                self.log.write(&format!("{step:?}: {error}"));
            }
        }
    }

    /// Make one change in herdr.
    fn apply(
        &mut self,
        step: &Step,
        agent_panes: &[AgentPane],
        snapshot: &Snapshot,
    ) -> Result<(), Error> {
        match step {
            Step::CloseWorkspace { workspace_id } => {
                herdr::close_workspace(workspace_id)?;
                self.workspaces.remove(workspace_id);
                self.log
                    .write(&format!("closed the workspace {workspace_id}"));
                Ok(())
            }
            Step::MovePane {
                pane_id,
                checkout,
                focus,
            } => {
                let source_workspace = agent_panes
                    .iter()
                    .find(|pane| &pane.pane_id == pane_id)
                    .map(|pane| pane.workspace_id.clone())
                    .unwrap_or_default();
                self.move_pane(pane_id, &source_workspace, checkout, *focus, snapshot)?;
                self.differences.remove(pane_id);
                Ok(())
            }
        }
    }

    /// Move a pane to the workspace of `checkout`.
    ///
    /// If a workspace shows this checkout, the pane goes to a new tab of
    /// it, or replaces its shell if the workspace is new (see
    /// `fresh_shell`). Else, herdr opens the workspace, and the pane
    /// replaces its first shell. herdr closes the old workspace if the
    /// pane was its last pane.
    fn move_pane(
        &mut self,
        pane_id: &str,
        source_workspace: &str,
        checkout: &Checkout,
        focus: bool,
        snapshot: &Snapshot,
    ) -> Result<(), Error> {
        let existing = self
            .workspaces
            .iter()
            .find(|(id, first)| {
                id.as_str() != source_workspace && first.root.as_ref() == Some(&checkout.root)
            })
            .map(|(id, _)| id.clone());
        if let Some(workspace_id) = existing {
            let age = self.first_seen.get(&workspace_id).map(Instant::elapsed);
            if let Some(shell) = fresh_shell(age, &workspace_id, &snapshot.panes) {
                herdr::move_pane_next_to(pane_id, &shell.tab_id, &shell.pane_id, focus)?;
                herdr::close_pane(&shell.pane_id)?;
                self.log.write(&format!(
                    "moved {pane_id} to the workspace {workspace_id}, in place of its new shell"
                ));
                return Ok(());
            }
            herdr::move_pane_to_new_tab(pane_id, &workspace_id, focus)?;
            self.log
                .write(&format!("moved {pane_id} to the workspace {workspace_id}"));
            return Ok(());
        }
        let opened = herdr::open_checkout(&checkout.root, &checkout.main_checkout(), false)?;
        if opened.workspace_id == source_workspace {
            // herdr already gives this checkout to the workspace of the pane.
            self.remember(&opened.workspace_id, checkout);
            return Ok(());
        }
        let is_new_workspace = !self.workspaces.contains_key(&opened.workspace_id);
        if is_new_workspace {
            herdr::move_pane_next_to(pane_id, &opened.tab_id, &opened.root_pane_id, focus)?;
            herdr::close_pane(&opened.root_pane_id)?;
        } else {
            herdr::move_pane_to_new_tab(pane_id, &opened.workspace_id, focus)?;
        }
        self.remember(&opened.workspace_id, checkout);
        self.log.write(&format!(
            "moved {pane_id} to the workspace {} of {}",
            opened.workspace_id,
            checkout.root.display()
        ));
        Ok(())
    }

    fn remember(&mut self, workspace_id: &str, checkout: &Checkout) {
        self.workspaces.insert(
            workspace_id.to_owned(),
            FirstCheckout {
                root: Some(checkout.root.clone()),
                common_dir: Some(checkout.common_dir.clone()),
                is_linked_worktree: checkout.root != checkout.main_checkout(),
            },
        );
    }
}

/// Start the thread of the merged pull requests. gh and `wt remove` take
/// several seconds: this thread keeps them out of the loop of the
/// snapshots, so the panes do not wait for the network.
fn start_prune_thread(view: Arc<Mutex<PruneView>>, log: Arc<Log>, settings: PruneSettings) {
    let thread_log = Arc::clone(&log);
    let started = thread::Builder::new()
        .name("prune_merged".to_owned())
        .spawn(move || prune_loop(&view, &thread_log, &settings));
    if let Err(error) = started {
        log.write(&format!("cannot start the thread prune_merged: {error}"));
    }
}

/// The settings of the thread of the merged pull requests.
#[derive(Clone, Debug)]
struct PruneSettings {
    /// The state folder of the plugin: the files of the daily question.
    state_dir: PathBuf,
    /// The days without activity of an old worktree (0: no question).
    stale_days: u32,
}

/// An old worktree without pull request, for the daily question.
#[derive(Clone, Debug)]
struct OldWorktree {
    main_checkout: PathBuf,
    path: PathBuf,
    branch: String,
    days: u64,
}

/// Select the old worktrees among the worktrees without pull request: no
/// agent, a question allowed again, no activity for `stale_days` days, and
/// no change that is not committed.
fn old_worktrees(
    main_checkout: &Path,
    worktrees: Vec<LinkedWorktree>,
    view: &Mutex<PruneView>,
    asked: &HashMap<PathBuf, u64>,
    stale_days: u32,
) -> Vec<OldWorktree> {
    let now = stale::now();
    worktrees
        .into_iter()
        .filter_map(|worktree| {
            if !lock(view).agents_in(&worktree.path).is_empty() {
                return None;
            }
            let path = canonical(&worktree.path);
            if !stale::may_ask(asked, &path, now, stale_days) {
                return None;
            }
            let activity = git::last_activity(&path)?;
            if !stale::is_stale(activity, now, stale_days) || !git::is_clean(&path) {
                return None;
            }
            Some(OldWorktree {
                main_checkout: main_checkout.to_path_buf(),
                path,
                branch: worktree.branch,
                days: now.saturating_sub(activity) / stale::DAY,
            })
        })
        .collect()
}

/// Ask about each old worktree in the popup confirm, one after the other,
/// in a new thread: an open popup must not stop the removals.
fn ask_about_old_worktrees(old: Vec<OldWorktree>, settings: PruneSettings, log: Arc<Log>) {
    if old.is_empty() {
        return;
    }
    let thread_log = Arc::clone(&log);
    let started = thread::Builder::new()
        .name("stale_days".to_owned())
        .spawn(move || {
            let log = thread_log;
            let mut asked = stale::read_asked(&settings.state_dir);
            for worktree in old {
                let reason = format!("No pull request, and no change for {} days.", worktree.days);
                if let Err(error) =
                    actions::ask_to_delete(&worktree.main_checkout, &worktree.path, &reason)
                {
                    log.write(&format!(
                        "cannot ask about the old worktree of {}: {}",
                        worktree.branch,
                        one_line(&error)
                    ));
                    return;
                }
                asked.insert(worktree.path.clone(), stale::now());
                stale::write_asked(&settings.state_dir, &asked);
                log.write(&format!(
                    "asked about the old worktree of {} ({} days)",
                    worktree.branch, worktree.days
                ));
            }
        });
    if let Err(error) = started {
        log.write(&format!("cannot start the thread stale_days: {error}"));
    }
}

/// Check the merged pull requests of each repository, every 5 minutes.
///
/// The thread writes each removal in the log. It writes an error only if
/// the previous pass did not write it: an error that stays gives one line,
/// not one line every 5 minutes.
fn prune_loop(view: &Mutex<PruneView>, log: &Arc<Log>, settings: &PruneSettings) {
    let mut previous_errors = HashSet::new();
    thread::sleep(PRUNE_FIRST_DELAY);
    loop {
        let repositories: Vec<PathBuf> = lock(view).repositories.iter().cloned().collect();
        let repositories: HashSet<PathBuf> = repositories
            .iter()
            .map(|repository| canonical(repository))
            .collect();
        let is_stale_pass = settings.stale_days > 0
            && stale::is_pass_due(stale::read_last_pass(&settings.state_dir), stale::now());
        let asked = if is_stale_pass {
            stale::read_asked(&settings.state_dir)
        } else {
            HashMap::new()
        };
        let mut old = Vec::new();
        let mut is_checked = false;
        let mut errors = HashSet::new();
        for repository in repositories {
            let Some(without_pull_request) = prune_repository(&repository, view, log, &mut errors)
            else {
                continue;
            };
            is_checked = true;
            if is_stale_pass {
                old.extend(old_worktrees(
                    &repository,
                    without_pull_request,
                    view,
                    &asked,
                    settings.stale_days,
                ));
            }
        }
        // A pass without a checked repository (no workspace yet, or gh
        // fails) does not count: the next pass tries again.
        if is_stale_pass && is_checked {
            stale::write_last_pass(&settings.state_dir, stale::now());
            ask_about_old_worktrees(old, settings.clone(), Arc::clone(log));
        }
        for error in &errors {
            if !previous_errors.contains(error) {
                log.write(error);
            }
        }
        previous_errors = errors;
        thread::sleep(PRUNE_INTERVAL);
    }
}

/// Remove the finished worktrees of one repository. Write each removal in
/// the log, and add each error to `errors`. Return the worktrees without
/// pull request, for the daily question, or None if the check failed.
///
/// A worktree where an agent must stay (see `agent_decision`) costs no
/// call to GitHub.
fn prune_repository(
    main_checkout: &Path,
    view: &Mutex<PruneView>,
    log: &Log,
    errors: &mut HashSet<String>,
) -> Option<Vec<LinkedWorktree>> {
    let must_stay = |checkout: &Path| {
        let agents = lock(view).agents_in(checkout);
        agent_decision(&agents, checkout) == AgentDecision::Wait
    };
    let check = match prune::check_worktrees(main_checkout, must_stay) {
        Ok(check) => check,
        Err(error) => {
            errors.insert(format!(
                "prune_merged {}: {}",
                main_checkout.display(),
                one_line(&error)
            ));
            return None;
        }
    };
    for worktree in &check.finished {
        if let Err(error) = finish_worktree(main_checkout, worktree, view, log) {
            errors.insert(format!("finish {}: {}", worktree.branch, one_line(&error)));
        }
    }
    Some(check.without_pull_request)
}

/// Remove one finished worktree and its branch.
///
/// The thread reads the view and the commit of the branch just before the
/// removal: during the calls to GitHub, an agent can enter the worktree,
/// or a commit can change the branch. If idle agents work in the worktree,
/// the thread first closes their panes. Then the observer closes the
/// workspace of the removed worktree (rule 2).
fn finish_worktree(
    main_checkout: &Path,
    worktree: &FinishedWorktree,
    view: &Mutex<PruneView>,
    log: &Log,
) -> Result<(), Error> {
    let agents = lock(view).agents_in(&worktree.path);
    let decision = agent_decision(&agents, &worktree.path);
    if decision == AgentDecision::Wait {
        return Ok(());
    }
    // Check the branch and the changes before an agent closes: if the
    // worktree must stay, its agents stay too.
    let current_commit = git::branch_commit(main_checkout, &worktree.branch);
    if !worktree.is_unchanged(current_commit.as_deref()) || !git::is_clean(&worktree.path) {
        return Ok(());
    }
    match decision {
        AgentDecision::Wait | AgentDecision::Remove => {}
        AgentDecision::CloseThenRemove => {
            for agent in &agents {
                herdr::close_pane(&agent.pane_id)?;
                log.write(&format!(
                    "closed the idle agent of {} (pane {}): {}",
                    worktree.branch,
                    agent.pane_id,
                    resume_command(agent)
                ));
            }
            if !wait_for_no_agent(view, &worktree.path) {
                return Ok(());
            }
        }
    }
    let current_commit = git::branch_commit(main_checkout, &worktree.branch);
    if !worktree.is_unchanged(current_commit.as_deref()) {
        return Ok(());
    }
    wt::remove_finished(main_checkout, &worktree.path)?;
    let number = worktree.pull_request.number;
    let message = match worktree.pull_request.state {
        PullRequestState::Closed => format!(
            "removed the worktree of {}: its pull request #{number} is closed. To get the branch back: git fetch origin pull/{number}/head:{}",
            worktree.branch, worktree.branch
        ),
        _ => format!(
            "removed the worktree of {}: its pull request #{number} is merged",
            worktree.branch
        ),
    };
    log.write(&message);
    Ok(())
}

/// Wait until the view shows no agent in `checkout`, for 10 seconds at
/// most. Return false if an agent stays.
fn wait_for_no_agent(view: &Mutex<PruneView>, checkout: &Path) -> bool {
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(500));
        if lock(view).agents_in(checkout).is_empty() {
            return true;
        }
    }
    false
}

/// The text of an error on one line of the log.
fn one_line(error: &Error) -> String {
    error.to_string().replace('\n', " ")
}

/// A small log file in the state folder of the plugin. The 2 threads of
/// the observer write in it.
struct Log {
    file: Mutex<Option<File>>,
}

impl Log {
    fn open(path: &Path) -> Self {
        let is_too_big =
            fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_LOG_SIZE_BYTES);
        let file = OpenOptions::new()
            .create(true)
            .append(!is_too_big)
            .write(true)
            .truncate(is_too_big)
            .open(path)
            .ok();
        Log {
            file: Mutex::new(file),
        }
    }

    fn write(&self, message: &str) {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(file) = file.as_mut() {
            // The log is only for a diagnosis: a failed write must not stop
            // the observer.
            let _ = writeln!(file, "{seconds} {message}");
        }
    }
}

/// The lock file: only one observer runs for each herdr server.
fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("observer.lock")
}

fn open_lock(state_dir: &Path) -> Result<File, Error> {
    fs::create_dir_all(state_dir).map_err(|error| {
        Error::Command(format!("cannot create {}: {error}", state_dir.display()))
    })?;
    let path = lock_path(state_dir);
    File::create(&path)
        .map_err(|error| Error::Command(format!("cannot create {}: {error}", path.display())))
}

/// The file with the path of the binary of the last `observe` command.
///
/// herdr installs each version of the plugin in a new folder, and keeps
/// the old folders. Thus the binary of a running observer never changes,
/// and the observer cannot see an update in it. herdr runs `observe` of
/// the active version when its server starts and when it detects an
/// agent: this file gives the running observer the path of this version.
fn wanted_binary_path(state_dir: &Path) -> PathBuf {
    state_dir.join("observer.binary")
}

/// Write the path of `binary` for the running observer. Write a temporary
/// file, then rename it: the observer never reads a partial path. Each
/// process has its own temporary file: 2 `observe` commands can run at
/// the same time.
fn write_wanted_binary(state_dir: &Path, binary: &Path) -> Result<(), Error> {
    let path = wanted_binary_path(state_dir);
    let temporary = state_dir.join(format!("observer.binary.{}.tmp", process::id()));
    let result = fs::write(&temporary, binary.as_os_str().as_bytes())
        .and_then(|()| fs::rename(&temporary, &path));
    if result.is_err() {
        // The temporary file can stay after a failed rename.
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| Error::Command(format!("cannot write {}: {error}", path.display())))
}

/// Read the path that the last `observe` command wrote, or None if the
/// file is missing or empty.
fn read_wanted_binary(state_dir: &Path) -> Option<PathBuf> {
    let bytes = fs::read(wanted_binary_path(state_dir)).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&bytes)))
}

/// Decide which binary the observer runs now, or None to keep its process.
///
/// - `current`: the canonical path of the binary of the observer.
/// - `wanted`: the canonical path of the binary of the last `observe`
///   command, if this binary exists.
/// - `is_rebuilt`: the binary of the observer has a new modification time,
///   for example after `scripts/install.sh` in a folder of `herdr plugin
///   link`.
fn binary_to_run(current: &Path, wanted: Option<&Path>, is_rebuilt: bool) -> Option<PathBuf> {
    match wanted {
        Some(wanted) if wanted != current => Some(wanted.to_path_buf()),
        _ if is_rebuilt => Some(current.to_path_buf()),
        _ => None,
    }
}

/// Start the observer in the background, if it does not run, and return.
///
/// herdr runs `[[startup]]` and `[[events]]` commands as short commands:
/// they must end. Thus this function starts a detached process
/// (`observe --run`) in a new process group, then returns at once. The
/// detached process takes the lock, so a second start does nothing.
///
/// The function also writes the path of its binary for the running
/// observer: if this observer runs another binary, it runs this one.
pub fn start(state_dir: &Path) -> Result<(), Error> {
    let lock = open_lock(state_dir)?;
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let written = write_wanted_binary(state_dir, &canonical(&executable));
    if lock.try_lock().is_err() {
        // An observer runs: only the file can give it this binary.
        return written;
    }
    // The detached process takes the lock itself.
    drop(lock);
    Command::new(executable)
        .args(["observe", "--run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| Error::Command(format!("cannot start the observer: {error}")))?;
    // The new observer runs this binary: it does not need the file.
    Ok(())
}

/// The modification time of a file, or None if the file does not exist.
fn modification_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// What the observer knows about its binary, to run a new binary.
struct BinaryWatch {
    /// The canonical path of the binary of the observer, saved at the
    /// start. On Linux, after a rebuild, `current_exe()` gives
    /// "<path> (deleted)": the observer reads the saved path.
    current: PathBuf,
    /// The modification time of `current` at the start, or after a failed
    /// `exec` of `current`.
    time: Option<SystemTime>,
    /// A wanted binary that `exec` refused: the observer does not try it
    /// again.
    refused: Option<PathBuf>,
}

impl BinaryWatch {
    fn new(current: PathBuf) -> Self {
        let time = modification_time(&current);
        BinaryWatch {
            current,
            time,
            refused: None,
        }
    }

    /// Return the binary that the observer runs now, or None to keep its
    /// process. A path to a binary that does not exist does not stop the
    /// observer.
    fn binary_to_run(&self, state_dir: &Path) -> Option<PathBuf> {
        let wanted = read_wanted_binary(state_dir)
            .map(|path| canonical(&path))
            .filter(|path| path.is_file() && self.refused.as_ref() != Some(path));
        let is_rebuilt =
            modification_time(&self.current).is_some_and(|time| Some(time) != self.time);
        binary_to_run(&self.current, wanted.as_deref(), is_rebuilt)
    }

    /// Remember a binary that `exec` refused.
    fn refuse(&mut self, binary: PathBuf) {
        if binary == self.current {
            // Try again only after the next rebuild.
            self.time = modification_time(&self.current);
        } else {
            self.refused = Some(binary);
        }
    }
}

/// Run the observer until herdr stops.
///
/// If another observer has the lock, return at once. The observer runs
/// the binary of the last `observe` command, or its own binary again if
/// this binary changed (see `binary_to_run`). With `prune_merged`, the
/// observer also starts the thread of the merged pull requests.
pub fn run(state_dir: &Path, prune_merged: bool, stale_days: u32) -> Result<(), Error> {
    let lock = open_lock(state_dir)?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let log = Arc::new(Log::open(&state_dir.join("observer.log")));
    let prune_view = prune_merged.then(|| Arc::new(Mutex::new(PruneView::default())));
    let mut observer = Observer {
        log: Arc::clone(&log),
        workspaces: HashMap::new(),
        folders: HashMap::new(),
        differences: HashSet::new(),
        prune_view: prune_view.clone(),
        first_seen: HashMap::new(),
        is_started: false,
    };
    observer.log.write("observer started");
    if let Some(view) = prune_view {
        observer.log.write("prune_merged: on");
        if stale_days > 0 {
            observer.log.write(&format!("stale_days: {stale_days}"));
        }
        let settings = PruneSettings {
            state_dir: state_dir.to_path_buf(),
            stale_days,
        };
        start_prune_thread(view, log, settings);
    }
    let mut binary_watch = match env::current_exe() {
        Ok(binary) => Some(BinaryWatch::new(canonical(&binary))),
        Err(error) => {
            observer
                .log
                .write(&format!("cannot find the binary of the observer: {error}"));
            None
        }
    };
    let mut failures = 0;
    loop {
        match herdr::snapshot() {
            Ok(snapshot) => {
                failures = 0;
                observer.tick(&snapshot);
            }
            Err(error) => {
                failures += 1;
                if failures >= MAX_SNAPSHOT_FAILURES {
                    observer.log.write(&format!("observer stopped: {error}"));
                    return Ok(());
                }
            }
        }
        if let Some(watch) = binary_watch.as_mut() {
            if let Some(binary) = watch.binary_to_run(state_dir) {
                observer.log.write(&format!(
                    "new binary: {} -> {}",
                    watch.current.display(),
                    binary.display()
                ));
                // The lock file closes at the exec (close-on-exec), so the
                // new process takes the lock. If the exec fails, this
                // process keeps the lock and continues.
                let error = Command::new(&binary).args(["observe", "--run"]).exec();
                observer.log.write(&format!(
                    "cannot run the new binary {}: {error}",
                    binary.display()
                ));
                watch.refuse(binary);
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(root: &str) -> Checkout {
        Checkout {
            root: PathBuf::from(root),
            common_dir: PathBuf::from("/work/repo/.git"),
        }
    }

    fn agent_in(workspace_id: &str, folder_checkout: Option<Checkout>) -> AgentPane {
        AgentPane {
            pane_id: "w1:p1".to_owned(),
            workspace_id: workspace_id.to_owned(),
            checkout: folder_checkout,
            is_focused: true,
            kind: "claude".to_owned(),
            status: Some("idle".to_owned()),
            session: Some("s1".to_owned()),
        }
    }

    fn workspace(workspace_id: &str, root: &str) -> KnownWorkspace {
        KnownWorkspace {
            workspace_id: workspace_id.to_owned(),
            checkout_root: Some(PathBuf::from(root)),
            common_dir: Some(PathBuf::from("/work/repo/.git")),
            is_linked_worktree: root != "/work/repo",
            has_agent: false,
            checkout_exists: true,
        }
    }

    fn previous(pane_id: &str) -> HashSet<String> {
        HashSet::from([pane_id.to_owned()])
    }

    #[test]
    fn an_agent_in_its_checkout_does_not_move() {
        let panes = [agent_in("w1", Some(checkout("/work/repo")))];

        let (steps, differences) =
            plan(&panes, &[workspace("w1", "/work/repo")], &previous("w1:p1"));

        assert!(steps.is_empty());
        assert!(differences.is_empty());
    }

    #[test]
    fn a_first_difference_waits_for_the_next_snapshot() {
        let panes = [agent_in("w1", Some(checkout("/work/repo.feature")))];

        let (steps, differences) = plan(&panes, &[workspace("w1", "/work/repo")], &HashSet::new());

        assert!(steps.is_empty());
        assert!(differences.contains("w1:p1"));
    }

    #[test]
    fn an_agent_in_a_worktree_moves_after_two_snapshots() {
        let panes = [agent_in("w1", Some(checkout("/work/repo.feature")))];

        let (steps, _) = plan(&panes, &[workspace("w1", "/work/repo")], &previous("w1:p1"));

        assert_eq!(
            steps,
            [Step::MovePane {
                pane_id: "w1:p1".to_owned(),
                checkout: checkout("/work/repo.feature"),
                focus: true,
            }]
        );
    }

    #[test]
    fn an_agent_in_another_repository_does_not_move() {
        let other_repository = Checkout {
            root: PathBuf::from("/work/other"),
            common_dir: PathBuf::from("/work/other/.git"),
        };
        let panes = [agent_in("w1", Some(other_repository))];

        let (steps, _) = plan(&panes, &[workspace("w1", "/work/repo")], &previous("w1:p1"));

        assert!(steps.is_empty());
    }

    #[test]
    fn an_agent_outside_git_does_not_move() {
        let panes = [agent_in("w1", None)];

        let (steps, _) = plan(&panes, &[workspace("w1", "/work/repo")], &previous("w1:p1"));

        assert!(steps.is_empty());
    }

    #[test]
    fn a_removed_worktree_without_agent_closes_its_workspace() {
        let mut removed = workspace("w2", "/work/repo.feature");
        removed.checkout_exists = false;

        let (steps, _) = plan(&[], &[removed], &HashSet::new());

        assert_eq!(
            steps,
            [Step::CloseWorkspace {
                workspace_id: "w2".to_owned()
            }]
        );
    }

    #[test]
    fn a_removed_worktree_with_an_agent_stays_open() {
        let mut removed = workspace("w2", "/work/repo.feature");
        removed.checkout_exists = false;
        removed.has_agent = true;

        let (steps, _) = plan(&[], &[removed], &HashSet::new());

        assert!(steps.is_empty());
    }

    fn first_checkout(root: Option<&str>) -> FirstCheckout {
        FirstCheckout {
            root: root.map(PathBuf::from),
            common_dir: root.map(|_| PathBuf::from("/work/repo/.git")),
            is_linked_worktree: root.is_some_and(|root| root != "/work/repo"),
        }
    }

    #[test]
    fn an_agent_has_its_checkout_and_the_checkout_of_its_workspace() {
        let workspaces =
            HashMap::from([("w1".to_owned(), first_checkout(Some("/work/repo.feature")))]);
        let panes = [agent_in("w1", Some(checkout("/work/repo.other")))];

        let agents = view_agents(&panes, &workspaces, &HashSet::from(["w1".to_owned()]));

        assert_eq!(
            agents[0].folder_root,
            Some(PathBuf::from("/work/repo.other"))
        );
        assert_eq!(
            agents[0].workspace_root,
            Some(PathBuf::from("/work/repo.feature"))
        );
        assert!(agents[0].is_workspace_focused);
        let view = PruneView {
            repositories: HashSet::new(),
            agents,
        };
        assert_eq!(view.agents_in(Path::new("/work/repo.feature")).len(), 1);
        assert_eq!(view.agents_in(Path::new("/work/repo.other")).len(), 1);
        assert!(view.agents_in(Path::new("/work/repo")).is_empty());
    }

    const FEATURE: &str = "/work/repo.feature";

    fn view_agent(status: &str, is_workspace_focused: bool) -> ViewAgent {
        ViewAgent {
            pane_id: "w2:p1".to_owned(),
            kind: "claude".to_owned(),
            status: Some(status.to_owned()),
            session: Some("s1".to_owned()),
            folder_root: Some(PathBuf::from(FEATURE)),
            workspace_root: Some(PathBuf::from(FEATURE)),
            is_workspace_focused,
        }
    }

    fn decide(agents: &[ViewAgent]) -> AgentDecision {
        agent_decision(agents, Path::new(FEATURE))
    }

    #[test]
    fn a_finished_worktree_without_agent_is_removed() {
        assert_eq!(decide(&[]), AgentDecision::Remove);
    }

    #[test]
    fn idle_or_done_agents_are_closed_then_the_worktree_is_removed() {
        let agents = [view_agent("idle", false), view_agent("done", false)];

        assert_eq!(decide(&agents), AgentDecision::CloseThenRemove);
    }

    #[test]
    fn a_working_blocked_or_unknown_agent_keeps_the_worktree() {
        for status in ["working", "blocked", "unknown"] {
            let agents = [view_agent("idle", false), view_agent(status, false)];

            assert_eq!(decide(&agents), AgentDecision::Wait, "{status}");
        }
    }

    #[test]
    fn an_idle_agent_in_the_focused_workspace_keeps_the_worktree() {
        assert_eq!(decide(&[view_agent("idle", true)]), AgentDecision::Wait);
    }

    fn pane(pane_id: &str, workspace_id: &str, agent: Option<&str>) -> Pane {
        serde_json::from_value(serde_json::json!({
            "pane_id": pane_id,
            "workspace_id": workspace_id,
            "tab_id": format!("{workspace_id}:t1"),
            "agent": agent,
        }))
        .unwrap()
    }

    const YOUNG: Option<Duration> = Some(Duration::from_secs(5));

    #[test]
    fn the_only_shell_of_a_new_workspace_is_fresh() {
        let panes = [
            pane("w1:p1", "w1", Some("claude")),
            pane("w2:p1", "w2", None),
        ];

        assert_eq!(
            fresh_shell(YOUNG, "w2", &panes),
            Some(FreshShell {
                pane_id: "w2:p1".to_owned(),
                tab_id: "w2:t1".to_owned(),
            })
        );
    }

    #[test]
    fn an_old_or_unknown_workspace_keeps_its_shell() {
        let panes = [pane("w2:p1", "w2", None)];

        assert_eq!(fresh_shell(Some(FRESH_WORKSPACE_TIME), "w2", &panes), None);
        assert_eq!(fresh_shell(None, "w2", &panes), None);
    }

    #[test]
    fn a_workspace_with_2_panes_keeps_its_shells() {
        let panes = [pane("w2:p1", "w2", None), pane("w2:p2", "w2", None)];

        assert_eq!(fresh_shell(YOUNG, "w2", &panes), None);
    }

    #[test]
    fn an_agent_is_not_a_fresh_shell() {
        let panes = [pane("w2:p1", "w2", Some("codex"))];

        assert_eq!(fresh_shell(YOUNG, "w2", &panes), None);
    }

    #[test]
    fn an_idle_agent_in_another_folder_is_not_closed() {
        // Its workspace shows the worktree, but it works in another
        // repository.
        let mut agent = view_agent("idle", false);
        agent.folder_root = Some(PathBuf::from("/work/other"));

        assert_eq!(decide(&[agent]), AgentDecision::Wait);
    }

    #[test]
    fn the_resume_command_depends_on_the_agent() {
        let mut agent = view_agent("idle", false);
        assert_eq!(resume_command(&agent), "claude --resume s1");
        agent.kind = "codex".to_owned();
        assert_eq!(resume_command(&agent), "codex resume s1");
    }

    #[test]
    fn the_repositories_are_the_main_checkouts_of_the_workspaces() {
        let workspaces = HashMap::from([
            ("w1".to_owned(), first_checkout(Some("/work/repo"))),
            ("w2".to_owned(), first_checkout(Some("/work/repo.feature"))),
            ("w3".to_owned(), first_checkout(None)),
        ]);

        assert_eq!(
            repositories(&workspaces),
            HashSet::from([PathBuf::from("/work/repo")])
        );
    }

    #[test]
    fn the_main_checkout_never_closes() {
        let mut main = workspace("w1", "/work/repo");
        main.checkout_exists = false;

        let (steps, _) = plan(&[], &[main], &HashSet::new());

        assert!(steps.is_empty());
    }

    const OLD_BINARY: &str = "/plugins/herdr-worktrunk/1-0/checkout/bin/herdr-worktrunk";
    const NEW_BINARY: &str = "/plugins/herdr-worktrunk/2-0/checkout/bin/herdr-worktrunk";

    #[test]
    fn the_observer_runs_the_binary_of_a_new_version() {
        let binary = binary_to_run(Path::new(OLD_BINARY), Some(Path::new(NEW_BINARY)), false);

        assert_eq!(binary, Some(PathBuf::from(NEW_BINARY)));
    }

    #[test]
    fn the_observer_keeps_its_process_if_nothing_changed() {
        let binary = binary_to_run(Path::new(OLD_BINARY), Some(Path::new(OLD_BINARY)), false);

        assert_eq!(binary, None);
    }

    #[test]
    fn a_rebuilt_binary_runs_again_without_a_wanted_binary() {
        let binary = binary_to_run(Path::new(OLD_BINARY), None, true);

        assert_eq!(binary, Some(PathBuf::from(OLD_BINARY)));
    }

    #[test]
    fn a_rebuilt_binary_runs_again_if_it_is_the_wanted_binary() {
        let binary = binary_to_run(Path::new(OLD_BINARY), Some(Path::new(OLD_BINARY)), true);

        assert_eq!(binary, Some(PathBuf::from(OLD_BINARY)));
    }

    /// A new empty folder for one test, in the temporary folder.
    fn test_folder(name: &str) -> PathBuf {
        let folder = env::temp_dir().join(format!(
            "herdr-worktrunk-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn the_wanted_binary_is_read_back() {
        let folder = test_folder("read-back");

        write_wanted_binary(&folder, Path::new(NEW_BINARY)).unwrap();

        assert_eq!(read_wanted_binary(&folder), Some(PathBuf::from(NEW_BINARY)));
        assert!(!folder.join("observer.binary.tmp").exists());
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_refused_binary_is_not_tried_again() {
        let folder = test_folder("refused");
        let current = canonical(&folder).join("current");
        let new = canonical(&folder).join("new");
        let newer = canonical(&folder).join("newer");
        for binary in [&current, &new, &newer] {
            fs::write(binary, "").unwrap();
        }
        let mut watch = BinaryWatch::new(current.clone());
        write_wanted_binary(&folder, &new).unwrap();
        assert_eq!(watch.binary_to_run(&folder), Some(new.clone()));

        watch.refuse(new);

        assert_eq!(watch.binary_to_run(&folder), None);
        write_wanted_binary(&folder, &newer).unwrap();
        assert_eq!(watch.binary_to_run(&folder), Some(newer));
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_rebuilt_binary_that_exec_refused_waits_for_the_next_rebuild() {
        let folder = test_folder("rebuilt");
        let current = canonical(&folder).join("current");
        fs::write(&current, "").unwrap();
        let mut watch = BinaryWatch::new(current.clone());
        let later = SystemTime::now() + Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&current)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(watch.binary_to_run(&folder), Some(current.clone()));

        watch.refuse(current);

        assert_eq!(watch.binary_to_run(&folder), None);
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_missing_or_empty_file_gives_no_wanted_binary() {
        let folder = test_folder("missing");

        assert_eq!(read_wanted_binary(&folder), None);
        fs::write(wanted_binary_path(&folder), "").unwrap();
        assert_eq!(read_wanted_binary(&folder), None);
        fs::remove_dir_all(&folder).unwrap();
    }
}
