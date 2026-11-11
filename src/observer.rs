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
//! the finished pull requests every 5 minutes (see `prune`). It also
//! removes the worktrees whose branch is in the default branch, with the
//! rule of the cleaner (`leftover::merged_branch`). It skips a worktree
//! where an agent works. Then the rule 2 closes the workspace.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::forge::PullRequestState;
use crate::git::{self, Checkout, LinkedWorktree};
use crate::herdr::{self, Error, Pane, Snapshot};
use crate::leftover;
use crate::prune::{self, FinishedWorktree, MergedWorktree};
use crate::restore;
use crate::stale::{self, OldWorktree};
use crate::sync::lock;
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
    /// The workspaces that the observer saw, also those whose checkout
    /// it could not read.
    seen: HashSet<String>,
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

/// Return true if the move of `pane_id` out of the workspace
/// `workspace_id` empties a workspace of a main checkout.
///
/// herdr closes a workspace without pane. The workspace of a main checkout
/// is the first workspace of the group of its project: without it, the
/// workspaces of the worktrees leave the group. Thus the observer opens a
/// shell in it first.
fn empties_main_workspace(
    first: Option<&FirstCheckout>,
    workspace_id: &str,
    pane_id: &str,
    panes: &[Pane],
) -> bool {
    let is_main = first.is_some_and(|first| first.root.is_some() && !first.is_linked_worktree);
    let mut panes = panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace_id);
    let is_last_pane =
        panes.next().is_some_and(|only| only.pane_id == pane_id) && panes.next().is_none();
    is_main && is_last_pane
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

/// Record in `panes` that `pane_id` is now in `workspace_id`.
fn set_workspace(panes: &mut [Pane], pane_id: &str, workspace_id: &str) {
    for pane in panes.iter_mut().filter(|pane| pane.pane_id == pane_id) {
        pane.workspace_id = workspace_id.to_owned();
    }
}

/// The shell that the observer opened in `workspace_id`, for the copy of
/// the panes of a snapshot. Its ID is not a herdr ID: no change uses it.
fn opened_shell(workspace_id: &str, folder: &Path) -> Pane {
    Pane {
        pane_id: format!("opened-shell-{workspace_id}"),
        workspace_id: workspace_id.to_owned(),
        tab_id: String::new(),
        agent: None,
        agent_status: None,
        agent_session: None,
        cwd: Some(folder.to_path_buf()),
        foreground_cwd: None,
        focused: false,
    }
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
    /// The workspace of the agent shows a main checkout. If the pane of
    /// the agent is its last pane, a close empties it, and herdr closes it.
    is_in_main_workspace: bool,
}

impl ViewAgent {
    /// Return true if the folder of the agent is in `checkout`.
    fn works_in(&self, checkout: &Path) -> bool {
        self.folder_root
            .as_deref()
            .is_some_and(|root| git::canonical(root) == git::canonical(checkout))
    }

    /// Return true if the agent waits in the worktree `checkout`, as the
    /// cleaner sees it (`leftover::WorktreeAgent::may_close`): idle or
    /// done, its folder in the worktree, its pane in the workspace of the
    /// worktree, and this workspace without the focus. The cleaner shows
    /// such a worktree in the group "Waiting".
    fn waits_in(&self, checkout: &Path) -> bool {
        matches!(self.status.as_deref(), Some("idle" | "done"))
            && !self.is_workspace_focused
            && self.works_in(checkout)
            && self
                .workspace_root
                .as_deref()
                .is_some_and(|root| git::canonical(root) == git::canonical(checkout))
    }

    /// Return true if the folder or the workspace of the agent is in
    /// `checkout`.
    fn uses(&self, checkout: &Path) -> bool {
        self.works_in(checkout)
            || self
                .workspace_root
                .as_deref()
                .is_some_and(|root| git::canonical(root) == git::canonical(checkout))
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
    /// An agent works, waits for an answer, has the focus, works in
    /// another folder, or stays in the workspace of a main checkout: keep
    /// the worktree until the next pass.
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
            && !agent.is_in_main_workspace
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
    leftover::resume_command(&agent.kind, agent.session.as_deref())
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
            is_in_main_workspace: workspaces
                .get(&pane.workspace_id)
                .is_some_and(|first| first.root.is_some() && !first.is_linked_worktree),
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
        self.seen.retain(|id| live.contains(id.as_str()));
        for workspace in &snapshot.workspaces {
            // The workspaces of the first snapshot are not new: the user
            // can use their shells. A workspace whose checkout could not be
            // read keeps its first date.
            let is_new = self.seen.insert(workspace.workspace_id.clone());
            if is_new && self.is_started {
                self.first_seen
                    .insert(workspace.workspace_id.clone(), Instant::now());
            }
            if self.workspaces.contains_key(&workspace.workspace_id) {
                continue;
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
        // The changes of this snapshot update this copy: a second move in
        // the same snapshot sees the first one.
        let mut panes = snapshot.panes.clone();
        for step in steps {
            if let Err(error) = self.apply(&step, &agent_panes, &mut panes) {
                self.log.write(&format!("{step:?}: {error}"));
            }
        }
    }

    /// Make one change in herdr.
    fn apply(
        &mut self,
        step: &Step,
        agent_panes: &[AgentPane],
        panes: &mut Vec<Pane>,
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
                self.move_pane(pane_id, &source_workspace, checkout, *focus, panes)?;
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
    ///
    /// `panes` is the copy of the panes of the snapshot: each change of
    /// this function updates it.
    fn move_pane(
        &mut self,
        pane_id: &str,
        source_workspace: &str,
        checkout: &Checkout,
        focus: bool,
        panes: &mut Vec<Pane>,
    ) -> Result<(), Error> {
        let source_first = self.workspaces.get(source_workspace);
        if empties_main_workspace(source_first, source_workspace, pane_id, panes) {
            if let Some(root) = source_first.and_then(|first| first.root.clone()) {
                herdr::open_shell_tab(source_workspace, &root)?;
                panes.push(opened_shell(source_workspace, &root));
                self.log.write(&format!(
                    "opened a shell in the workspace {source_workspace}: it keeps the group of its project"
                ));
            }
        }
        let existing = self
            .workspaces
            .iter()
            .find(|(id, first)| {
                id.as_str() != source_workspace && first.root.as_ref() == Some(&checkout.root)
            })
            .map(|(id, _)| id.clone());
        if let Some(workspace_id) = existing {
            let age = self.first_seen.get(&workspace_id).map(Instant::elapsed);
            let shell = fresh_shell(age, &workspace_id, panes)
                .filter(|shell| herdr::shell_is_idle(&shell.pane_id).unwrap_or(false));
            if let Some(shell) = shell {
                herdr::move_pane_next_to(pane_id, &shell.tab_id, &shell.pane_id, focus)?;
                set_workspace(panes, pane_id, &workspace_id);
                herdr::close_pane(&shell.pane_id)?;
                panes.retain(|pane| pane.pane_id != shell.pane_id);
                self.log.write(&format!(
                    "moved {pane_id} to the workspace {workspace_id}, in place of its new shell"
                ));
                return Ok(());
            }
            herdr::move_pane_to_new_tab(pane_id, &workspace_id, focus)?;
            set_workspace(panes, pane_id, &workspace_id);
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
        set_workspace(panes, pane_id, &opened.workspace_id);
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
    /// The state folder of the plugin: the file of the notifications of
    /// the old worktrees.
    state_dir: PathBuf,
    /// The days without activity of an old worktree (0: no notification).
    stale_days: u32,
}

/// Select the old worktrees among the worktrees without pull request: no
/// agent that works (an idle agent can wait there), a question allowed
/// again, no activity for `stale_days` days, and no change that is not
/// committed. The cleaner shows a worktree with an idle agent in the group
/// "Waiting", and never deletes it.
fn old_worktrees(
    main_checkout: &Path,
    worktrees: Vec<LinkedWorktree>,
    view: &Mutex<PruneView>,
    asked: &HashMap<PathBuf, u64>,
    stale_days: u32,
) -> Vec<OldWorktree> {
    let is_busy = |checkout: &Path| {
        let agents = lock(view).agents_in(checkout);
        !agents.iter().all(|agent| agent.waits_in(checkout))
    };
    stale::select_old(main_checkout, worktrees, is_busy, Some(asked), stale_days)
}

/// Name the new old worktrees in one herdr notification, and record the
/// time: the thread names them again only after `stale_days` days. The
/// paths in `waiting` have an idle agent.
fn notify_old_worktrees(
    old: &[OldWorktree],
    waiting: &HashSet<PathBuf>,
    settings: &PruneSettings,
    log: &Log,
) {
    let (title, body) = old_worktrees_notification(old, waiting);
    if let Err(error) = herdr::show_notification(&title, &body) {
        log.write(&format!(
            "cannot show the notification of the old worktrees: {}",
            one_line(&error)
        ));
        return;
    }
    let mut asked = stale::read_asked(&settings.state_dir);
    let now = stale::now();
    for worktree in old {
        asked.insert(worktree.path.clone(), now);
    }
    stale::write_asked(&settings.state_dir, &asked);
    let branches: Vec<&str> = old
        .iter()
        .map(|worktree| worktree.branch.as_str())
        .collect();
    log.write(&format!(
        "named the worktrees to clean: {}",
        branches.join(", ")
    ));
}

/// The title and the body of the notification of the old worktrees: one
/// line for each worktree, then how to clean them.
fn old_worktrees_notification(old: &[OldWorktree], waiting: &HashSet<PathBuf>) -> (String, String) {
    let title = if old.len() == 1 {
        "1 worktree to clean".to_owned()
    } else {
        format!("{} worktrees to clean", old.len())
    };
    let mut lines: Vec<String> = old
        .iter()
        .map(|worktree| {
            let project = git::folder_name(&worktree.main_checkout);
            let days = if worktree.days == 1 {
                "1 day".to_owned()
            } else {
                format!("{} days", worktree.days)
            };
            if waiting.contains(&worktree.path) {
                format!(
                    "{} ({project}): an idle agent waits, no activity for {days}",
                    worktree.branch
                )
            } else {
                format!(
                    "{} ({project}): no pull request, no activity for {days}",
                    worktree.branch
                )
            }
        })
        .collect();
    lines.push("Open the cleaner: action clean of herdr-worktrunk.".to_owned());
    (title, lines.join("\n"))
}

/// Check the merged pull requests of each repository, every 5 minutes.
///
/// The thread writes each removal in the log. It writes a notice (an
/// error, or a worktree that an agent keeps) only if the previous pass did
/// not write it: a notice that stays gives one line, not one line every 5
/// minutes.
fn prune_loop(view: &Mutex<PruneView>, log: &Arc<Log>, settings: &PruneSettings) {
    let mut previous_notices = HashSet::new();
    thread::sleep(PRUNE_FIRST_DELAY);
    loop {
        let repositories: Vec<PathBuf> = lock(view).repositories.iter().cloned().collect();
        let repositories: HashSet<PathBuf> = repositories
            .iter()
            .map(|repository| git::canonical(repository))
            .collect();
        let is_stale_check = settings.stale_days > 0;
        let asked = if is_stale_check {
            stale::read_asked(&settings.state_dir)
        } else {
            HashMap::new()
        };
        let mut old = Vec::new();
        let mut notices = HashSet::new();
        for repository in repositories {
            let Some(without_pull_request) = prune_repository(&repository, view, log, &mut notices)
            else {
                continue;
            };
            if is_stale_check {
                old.extend(old_worktrees(
                    &repository,
                    without_pull_request,
                    view,
                    &asked,
                    settings.stale_days,
                ));
            }
        }
        if !old.is_empty() {
            let waiting: HashSet<PathBuf> = old
                .iter()
                .filter(|worktree| !lock(view).agents_in(&worktree.path).is_empty())
                .map(|worktree| worktree.path.clone())
                .collect();
            notify_old_worktrees(&old, &waiting, settings, log);
        }
        for notice in &notices {
            if !previous_notices.contains(notice) {
                log.write(notice);
            }
        }
        previous_notices = notices;
        thread::sleep(PRUNE_INTERVAL);
    }
}

/// Remove the finished worktrees of one repository. Write each removal in
/// the log, and add each error and each worktree that an agent keeps to
/// `notices`. Return the worktrees without pull request, for the
/// notification of the old worktrees, or None if the check failed for all
/// the branches.
///
/// The thread asks the forge also about a worktree where an agent works:
/// the log then tells that the agent keeps a finished worktree. The cost
/// is one call to the forge for each of these worktrees, every 5 minutes.
fn prune_repository(
    main_checkout: &Path,
    view: &Mutex<PruneView>,
    log: &Log,
    notices: &mut HashSet<String>,
) -> Option<Vec<LinkedWorktree>> {
    let check = match prune::check_worktrees(main_checkout) {
        Ok(check) => check,
        Err(error) => {
            notices.insert(format!(
                "prune_merged {}: {}",
                main_checkout.display(),
                one_line(&error)
            ));
            return None;
        }
    };
    // One notice for the repository: a forge that does not answer gives
    // one line, not one line for each branch.
    if let Some((branch, error)) = check.errors.first() {
        notices.insert(format!(
            "prune_merged {}: {} not checked. {branch}: {}",
            main_checkout.display(),
            branches_text(check.errors.len()),
            one_line(error)
        ));
    }
    for worktree in &check.finished {
        if let Err(error) = finish_worktree(main_checkout, worktree, view, log, notices) {
            notices.insert(format!("finish {}: {}", worktree.branch, one_line(&error)));
        }
    }
    let mut without_pull_request = check.without_pull_request;
    for worktree in &check.merged {
        let is_kept_by_rule = match finish_merged_worktree(main_checkout, worktree, view, log) {
            Ok(is_kept_by_rule) => is_kept_by_rule,
            Err(error) => {
                notices.insert(format!("finish {}: {}", worktree.branch, one_line(&error)));
                false
            }
        };
        // A merged worktree that the rule keeps, without a pull request,
        // can be an old worktree too: for example a fork that
        // never committed.
        if is_kept_by_rule && !worktree.has_pull_request {
            without_pull_request.push(LinkedWorktree {
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
            });
        }
    }
    // Without any checked branch, the list of the worktrees without pull
    // request is empty.
    if check.checked == 0 && !check.errors.is_empty() {
        return None;
    }
    Some(without_pull_request)
}

/// A number of branches: `1 branch`, `3 branches`.
fn branches_text(count: usize) -> String {
    if count == 1 {
        "1 branch".to_owned()
    } else {
        format!("{count} branches")
    }
}

/// Remove one finished worktree and its branch.
///
/// The thread reads the view and the commit of the branch just before the
/// removal: during the calls to the forge, an agent can enter the worktree,
/// or a commit can change the branch. If idle agents work in the worktree,
/// the thread first closes their panes. Then the observer closes the
/// workspace of the removed worktree (rule 2).
fn finish_worktree(
    main_checkout: &Path,
    worktree: &FinishedWorktree,
    view: &Mutex<PruneView>,
    log: &Log,
    notices: &mut HashSet<String>,
) -> Result<(), Error> {
    let target = Target {
        main_checkout,
        path: &worktree.path,
        branch: &worktree.branch,
        commit: &worktree.commit,
        recover_command: Some(&worktree.recover_command),
    };
    let is_closed = worktree.pull_request.state == PullRequestState::Closed;
    let state = if is_closed { "closed" } else { "merged" };
    let closed_agents = match prepare_removal(&target, view, log, |_| true)? {
        Preparation::Ready(closed_agents) => closed_agents,
        Preparation::AgentStays => {
            notices.insert(format!(
                "kept the worktree of {}: its {} is {state}, but an agent works in it, or its workspace has the focus",
                worktree.branch, worktree.reference
            ));
            return Ok(());
        }
        Preparation::Refused | Preparation::Kept => return Ok(()),
    };
    wt::remove_finished(main_checkout, &worktree.path)?;
    let why = if is_closed {
        format!(
            "its {} is closed. To get the branch back: {}",
            worktree.reference, worktree.recover_command
        )
    } else {
        format!("its {} is merged", worktree.reference)
    };
    report_removal(log, &worktree.branch, &why, &closed_agents);
    Ok(())
}

/// Remove one worktree whose branch is in the default branch, if the rule
/// `leftover::merged_branch` allows it: with its own commits, at once
/// (idle agents close first). Without own commit, only without agent, after
/// 1 day without activity. wt deletes the branch only if it sees it merged.
///
/// Return true if the rule or an agent keeps the worktree: not a change, a
/// late view of herdr or a removal.
fn finish_merged_worktree(
    main_checkout: &Path,
    worktree: &MergedWorktree,
    view: &Mutex<PruneView>,
    log: &Log,
) -> Result<bool, Error> {
    let target = Target {
        main_checkout,
        path: &worktree.path,
        branch: &worktree.branch,
        commit: &worktree.commit,
        recover_command: None,
    };
    // The facts of the rule cost 1 to 3 calls to git: the thread reads
    // them only after the cheap checks, and only once.
    let history = OnceCell::new();
    let may_remove = |agents| {
        let history = *history.get_or_init(|| {
            leftover::History::of(git::has_own_commits(main_checkout, &worktree.branch))
        });
        let days = match history {
            leftover::History::OwnCommits => None,
            _ => {
                git::last_activity(&worktree.path).map(|time| stale::days_since(time, stale::now()))
            }
        };
        leftover::merged_branch(history, days, agents) == Some(leftover::Reason::BranchMerged)
    };
    // An agent in a new worktree is the usual case: no notice. The agent
    // of a merge without pull request reports, then stops.
    let closed_agents = match prepare_removal(&target, view, log, may_remove)? {
        Preparation::Ready(closed_agents) => closed_agents,
        Preparation::AgentStays | Preparation::Refused => return Ok(true),
        Preparation::Kept => return Ok(false),
    };
    wt::remove_quietly(main_checkout, &worktree.path)?;
    let commits = match history.get() {
        Some(leftover::History::OwnCommits) => "its commits",
        _ => "its commit, and the worktree had no use for 1 day",
    };
    let kept_branch = if git::branch_commit(main_checkout, &worktree.branch).is_some() {
        ". wt kept the branch: it does not see it merged"
    } else {
        ""
    };
    let why = format!(
        "{} contains {commits}{kept_branch}",
        worktree.default_branch
    );
    report_removal(log, &worktree.branch, &why, &closed_agents);
    Ok(false)
}

/// Write a removal in the log, and show it to the user in a herdr
/// notification, with the commands that resume the closed agents.
fn report_removal(log: &Log, branch: &str, why: &str, closed_agents: &[String]) {
    log.write(&format!("removed the worktree of {branch}: {why}"));
    let body = removal_body(why, closed_agents);
    if let Err(error) = herdr::show_notification(&format!("Worktree {branch} removed"), &body) {
        log.write(&format!(
            "cannot show the notification: {}",
            one_line(&error)
        ));
    }
}

/// The text of the notification of a removal: the reason, then one line
/// for each closed agent. The reason keeps its case: it can start with the
/// name of a branch.
fn removal_body(why: &str, closed_agents: &[String]) -> String {
    let mut body = format!("Reason: {why}.");
    for resume in closed_agents {
        body.push_str(&format!("\nClosed agent, to resume it: {resume}"));
    }
    body.push_str("\nTo bring it back: action restore of herdr-worktrunk.");
    body
}

/// A worktree that the thread can remove, with the commit of its branch
/// at the check.
struct Target<'a> {
    main_checkout: &'a Path,
    path: &'a Path,
    branch: &'a str,
    commit: &'a str,
    /// The command that gets the branch back from the forge, for a closed
    /// pull request.
    recover_command: Option<&'a str>,
}

impl Target<'_> {
    /// Return true if the branch still has the checked commit.
    fn is_unchanged(&self) -> bool {
        git::branch_commit(self.main_checkout, self.branch).as_deref() == Some(self.commit)
    }
}

/// The result of the preparation of a removal.
#[derive(Debug, PartialEq, Eq)]
enum Preparation {
    /// The thread can remove the worktree now. The commands that resume
    /// the agents that it closed.
    Ready(Vec<String>),
    /// An agent works in the worktree, waits for an answer, or its
    /// workspace has the focus.
    AgentStays,
    /// The rule (`may_remove`) keeps the worktree.
    Refused,
    /// The branch, the changes or a late view of herdr keep the worktree.
    Kept,
}

/// Prepare the removal of a worktree.
///
/// The thread reads the view and the commit of the branch just before the
/// removal: during the calls to the forge, an agent can enter the
/// worktree, or a commit can change the branch. The worktree stays if an
/// agent must stay, if the branch has another commit, if the worktree has
/// changes, or if `may_remove` refuses the state of its agents. Else the
/// idle agents close first, then the thread checks the branch and the
/// changes again: an agent can write as it closes.
fn prepare_removal(
    target: &Target,
    view: &Mutex<PruneView>,
    log: &Log,
    may_remove: impl Fn(leftover::Agents) -> bool,
) -> Result<Preparation, Error> {
    // A worktree that the user restored stays for 1 day, while its branch
    // keeps the restored commit.
    if restore::is_kept(
        &crate::state_dir(),
        &git::canonical(target.path),
        target.commit,
    ) {
        return Ok(Preparation::Kept);
    }
    let agents = lock(view).agents_in(target.path);
    let state = match agent_decision(&agents, target.path) {
        AgentDecision::Wait => return Ok(Preparation::AgentStays),
        AgentDecision::Remove => leftover::Agents::None,
        AgentDecision::CloseThenRemove => leftover::Agents::Idle,
    };
    // Check the branch and the changes before an agent closes: if the
    // worktree must stay, its agents stay too. The cheap checks first.
    if !target.is_unchanged() || !git::is_clean(target.path) {
        return Ok(Preparation::Kept);
    }
    if !may_remove(state) {
        return Ok(Preparation::Refused);
    }
    restore::record(
        &crate::state_dir(),
        &restore::Removal {
            time: stale::now(),
            main_checkout: target.main_checkout.to_owned(),
            path: target.path.to_owned(),
            branch: target.branch.to_owned(),
            commit: target.commit.to_owned(),
            agents: agents
                .iter()
                .filter_map(|agent| {
                    restore::RemovedAgent::of(&agent.kind, agent.session.as_deref())
                })
                .collect(),
            recover_command: target.recover_command.map(str::to_owned),
        },
    );
    let mut closed_agents = Vec::new();
    if state == leftover::Agents::Idle {
        for agent in &agents {
            herdr::close_pane(&agent.pane_id)?;
            let resume = resume_command(agent);
            log.write(&format!(
                "closed the idle agent of {} (pane {}): {resume}",
                target.branch, agent.pane_id
            ));
            closed_agents.push(resume);
        }
        if !wait_for_no_agent(view, target.path) {
            // The agents closed, but the view of herdr is late: the next
            // pass removes the worktree.
            log.write(&format!(
                "closed the agents of {}, but herdr still shows one: the worktree stays until the next pass",
                target.branch
            ));
            return Ok(Preparation::Kept);
        }
        if !target.is_unchanged() || !git::is_clean(target.path) {
            return Ok(Preparation::Kept);
        }
    }
    Ok(Preparation::Ready(closed_agents))
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
        let mut file = lock(&self.file);
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
    let written = write_wanted_binary(state_dir, &git::canonical(&executable));
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

/// The time without change after which a rebuilt binary is complete. A
/// copy or a link can write the binary for a moment: an `exec` of a
/// partial file stops the observer.
const REBUILD_SETTLE_TIME: Duration = Duration::from_secs(2);

/// Return true if the binary changed since `start_time`, and did not
/// change for `REBUILD_SETTLE_TIME` before `now`.
fn is_settled_rebuild(
    time: Option<SystemTime>,
    start_time: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    time.is_some_and(|time| {
        Some(time) != start_time
            && now
                .duration_since(time)
                .is_ok_and(|age| age >= REBUILD_SETTLE_TIME)
    })
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
            .map(|path| git::canonical(&path))
            .filter(|path| path.is_file() && self.refused.as_ref() != Some(path));
        let is_rebuilt = is_settled_rebuild(
            modification_time(&self.current),
            self.time,
            SystemTime::now(),
        );
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
        seen: HashSet::new(),
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
        Ok(binary) => Some(BinaryWatch::new(git::canonical(&binary))),
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
    use crate::test_support::{git_in, git_output, test_folder};

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
            is_in_main_workspace: false,
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
    fn an_idle_agent_in_a_main_workspace_keeps_the_worktree() {
        let agent = ViewAgent {
            is_in_main_workspace: true,
            ..view_agent("idle", false)
        };

        assert_eq!(decide(&[agent]), AgentDecision::Wait);
    }

    #[test]
    fn a_rebuilt_binary_runs_only_after_2_seconds_without_change() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let rebuilt = start + Duration::from_secs(50);

        assert!(!is_settled_rebuild(
            Some(start),
            Some(start),
            rebuilt + Duration::from_secs(9)
        ));
        assert!(!is_settled_rebuild(
            Some(rebuilt),
            Some(start),
            rebuilt + Duration::from_secs(1)
        ));
        assert!(is_settled_rebuild(
            Some(rebuilt),
            Some(start),
            rebuilt + Duration::from_secs(2)
        ));
        assert!(!is_settled_rebuild(None, Some(start), rebuilt));
    }

    #[test]
    fn a_second_move_sees_the_first_one() {
        let first = FirstCheckout {
            root: Some(PathBuf::from("/work/repo")),
            common_dir: None,
            is_linked_worktree: false,
        };
        let mut panes = vec![
            pane("w1:p1", "w1", Some("claude")),
            pane("w1:p2", "w1", Some("claude")),
        ];

        assert!(!empties_main_workspace(Some(&first), "w1", "w1:p1", &panes));
        set_workspace(&mut panes, "w1:p1", "w2");
        assert!(empties_main_workspace(Some(&first), "w1", "w1:p2", &panes));
        panes.push(opened_shell("w1", Path::new("/work/repo")));
        assert!(!empties_main_workspace(Some(&first), "w1", "w1:p2", &panes));
    }

    #[test]
    fn a_workspace_that_got_an_agent_has_no_fresh_shell() {
        let mut panes = vec![
            pane("w2:p1", "w2", None),
            pane("w1:p1", "w1", Some("claude")),
        ];
        assert!(fresh_shell(YOUNG, "w2", &panes).is_some());

        set_workspace(&mut panes, "w1:p1", "w2");
        panes.retain(|pane| pane.pane_id != "w2:p1");

        assert_eq!(fresh_shell(YOUNG, "w2", &panes), None);
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
    fn the_last_pane_of_a_main_workspace_leaves_a_shell() {
        let main = first_checkout(Some("/work/repo"));
        let panes = [pane("w1:p1", "w1", Some("claude"))];

        assert!(empties_main_workspace(Some(&main), "w1", "w1:p1", &panes));
    }

    #[test]
    fn a_main_workspace_with_another_pane_stays_without_a_new_shell() {
        let main = first_checkout(Some("/work/repo"));
        let panes = [
            pane("w1:p1", "w1", Some("claude")),
            pane("w1:p2", "w1", None),
        ];

        assert!(!empties_main_workspace(Some(&main), "w1", "w1:p1", &panes));
    }

    #[test]
    fn a_worktree_workspace_can_close_after_its_last_pane() {
        let worktree = first_checkout(Some("/work/repo.feature"));
        let panes = [pane("w2:p1", "w2", Some("claude"))];

        assert!(!empties_main_workspace(
            Some(&worktree),
            "w2",
            "w2:p1",
            &panes
        ));
        assert!(!empties_main_workspace(None, "w2", "w2:p1", &panes));
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
        let current = git::canonical(&folder).join("current");
        let new = git::canonical(&folder).join("new");
        let newer = git::canonical(&folder).join("newer");
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
        let current = git::canonical(&folder).join("current");
        fs::write(&current, "").unwrap();
        let mut watch = BinaryWatch::new(current.clone());
        // A rebuild one minute ago: another time than at the start, and
        // no change for more than REBUILD_SETTLE_TIME.
        let rebuilt = SystemTime::now() - Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&current)
            .unwrap()
            .set_modified(rebuilt)
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

    #[test]
    fn a_target_is_unchanged_only_with_the_checked_commit() {
        let folder = test_folder("target-commit");
        git_in(&folder, &["init", "-q", "-b", "main"]);
        git_in(&folder, &["commit", "-q", "--allow-empty", "-m", "start"]);
        git_in(&folder, &["branch", "feat"]);
        let checked = git_output(&folder, &["rev-parse", "feat"]);
        let target = Target {
            main_checkout: &folder,
            path: &folder,
            branch: "feat",
            commit: &checked,
            recover_command: None,
        };

        assert!(target.is_unchanged());

        git_in(&folder, &["switch", "-q", "feat"]);
        git_in(&folder, &["commit", "-q", "--allow-empty", "-m", "new"]);
        assert!(!target.is_unchanged(), "a new commit");

        git_in(&folder, &["switch", "-q", "main"]);
        git_in(&folder, &["branch", "-q", "-D", "feat"]);
        assert!(!target.is_unchanged(), "a deleted branch");
    }

    #[test]
    fn the_notification_of_a_removal_gives_the_resume_commands() {
        assert_eq!(
            removal_body("its pull request #3 is merged", &[]),
            "Reason: its pull request #3 is merged.\nTo bring it back: action restore of herdr-worktrunk."
        );
        assert_eq!(
            removal_body(
                "main contains its commits",
                &["claude --resume abc".to_owned()]
            ),
            "Reason: main contains its commits.\nClosed agent, to resume it: claude --resume abc\nTo bring it back: action restore of herdr-worktrunk."
        );
    }

    #[test]
    fn an_agent_waits_only_in_the_workspace_of_its_worktree() {
        let worktree = Path::new("/work/app.feat");
        let agent = |status: &str, workspace: &str, focused: bool| ViewAgent {
            pane_id: "p1".to_owned(),
            kind: "claude".to_owned(),
            status: Some(status.to_owned()),
            session: None,
            folder_root: Some(PathBuf::from("/work/app.feat")),
            workspace_root: Some(PathBuf::from(workspace)),
            is_workspace_focused: focused,
            is_in_main_workspace: workspace == "/work/app",
        };

        assert!(agent("idle", "/work/app.feat", false).waits_in(worktree));
        assert!(agent("done", "/work/app.feat", false).waits_in(worktree));
        // The cleaner sees these agents as busy: no group Waiting.
        assert!(!agent("working", "/work/app.feat", false).waits_in(worktree));
        assert!(!agent("idle", "/work/app.feat", true).waits_in(worktree));
        assert!(!agent("idle", "/work/other", false).waits_in(worktree));
    }

    #[test]
    fn one_notification_names_the_worktrees_to_clean() {
        let old = |branch: &str, days: u64| OldWorktree {
            main_checkout: PathBuf::from("/work/app"),
            path: PathBuf::from(format!("/work/app.{branch}")),
            branch: branch.to_owned(),
            days,
        };
        let waiting = HashSet::from([PathBuf::from("/work/app.fork")]);

        let (title, body) =
            old_worktrees_notification(&[old("spike", 9), old("fork", 1)], &waiting);

        assert_eq!(title, "2 worktrees to clean");
        assert_eq!(
            body,
            "spike (app): no pull request, no activity for 9 days\n\
             fork (app): an idle agent waits, no activity for 1 day\n\
             Open the cleaner: action clean of herdr-worktrunk."
        );
        let (title, _) = old_worktrees_notification(&[old("spike", 9)], &HashSet::new());
        assert_eq!(title, "1 worktree to clean");
    }
}
