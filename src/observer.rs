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
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::git::{self, Checkout};
use crate::herdr::{self, Error, Snapshot};
use crate::prune;
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
}

/// What the thread of the merged pull requests reads from the observer.
/// The observer writes it at each snapshot. The paths are not canonical:
/// the thread makes them canonical, so the loop of the snapshots stays
/// cheap.
#[derive(Debug, Default)]
struct PruneView {
    /// The main checkout of each repository that a workspace shows.
    repositories: HashSet<PathBuf>,
    /// The checkouts where an agent works.
    busy_checkouts: HashSet<PathBuf>,
}

impl PruneView {
    fn is_busy(&self, checkout: &Path) -> bool {
        let checkout = canonical(checkout);
        self.busy_checkouts
            .iter()
            .any(|busy| canonical(busy) == checkout)
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

/// The checkouts where an agent works: the checkout of the folder of each
/// agent, and the checkout of the workspace of each agent. The actions
/// remove and merge refuse the same workspaces.
fn busy_checkouts(
    agent_panes: &[AgentPane],
    workspaces: &HashMap<String, FirstCheckout>,
) -> HashSet<PathBuf> {
    let mut busy = HashSet::new();
    for pane in agent_panes {
        if let Some(checkout) = &pane.checkout {
            busy.insert(checkout.root.clone());
        }
        let workspace_root = workspaces
            .get(&pane.workspace_id)
            .and_then(|first| first.root.clone());
        if let Some(root) = workspace_root {
            busy.insert(root);
        }
    }
    busy
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
        for workspace in &snapshot.workspaces {
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
            let new_view = PruneView {
                repositories: repositories(&self.workspaces),
                busy_checkouts: busy_checkouts(&agent_panes, &self.workspaces),
            };
            *lock(view) = new_view;
        }
        let (steps, differences) = plan(&agent_panes, &known, &self.differences);
        self.differences = differences;
        for step in steps {
            if let Err(error) = self.apply(&step, &agent_panes) {
                self.log.write(&format!("{step:?}: {error}"));
            }
        }
    }

    /// Make one change in herdr.
    fn apply(&mut self, step: &Step, agent_panes: &[AgentPane]) -> Result<(), Error> {
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
                self.move_pane(pane_id, &source_workspace, checkout, *focus)?;
                self.differences.remove(pane_id);
                Ok(())
            }
        }
    }

    /// Move a pane to the workspace of `checkout`.
    ///
    /// If a workspace shows this checkout, the pane goes to a new tab of
    /// it. Else, herdr opens the workspace, and the pane replaces its
    /// first shell. herdr closes the old workspace if the pane was its
    /// last pane.
    fn move_pane(
        &mut self,
        pane_id: &str,
        source_workspace: &str,
        checkout: &Checkout,
        focus: bool,
    ) -> Result<(), Error> {
        let existing = self
            .workspaces
            .iter()
            .find(|(id, first)| {
                id.as_str() != source_workspace && first.root.as_ref() == Some(&checkout.root)
            })
            .map(|(id, _)| id.clone());
        if let Some(workspace_id) = existing {
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
fn start_prune_thread(view: Arc<Mutex<PruneView>>, log: Arc<Log>) {
    let thread_log = Arc::clone(&log);
    let started = thread::Builder::new()
        .name("prune_merged".to_owned())
        .spawn(move || prune_loop(&view, &thread_log));
    if let Err(error) = started {
        log.write(&format!("cannot start the thread prune_merged: {error}"));
    }
}

/// Check the merged pull requests of each repository, every 5 minutes.
///
/// The thread writes each removal in the log. It writes an error only if
/// the previous pass did not write it: an error that stays gives one line,
/// not one line every 5 minutes.
fn prune_loop(view: &Mutex<PruneView>, log: &Log) {
    let mut previous_errors = HashSet::new();
    thread::sleep(PRUNE_FIRST_DELAY);
    loop {
        let repositories: Vec<PathBuf> = lock(view).repositories.iter().cloned().collect();
        let repositories: HashSet<PathBuf> = repositories
            .iter()
            .map(|repository| canonical(repository))
            .collect();
        let mut errors = HashSet::new();
        for repository in repositories {
            prune_repository(&repository, view, log, &mut errors);
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

/// Remove the merged worktrees of one repository. Write each removal in
/// the log, and add each error to `errors`.
///
/// Before each removal, ask the view again and read the commit of the
/// branch again: during the calls to GitHub, an agent can enter the
/// worktree, or a commit can change the branch. `wt remove` runs at once
/// after the second check.
fn prune_repository(
    main_checkout: &Path,
    view: &Mutex<PruneView>,
    log: &Log,
    errors: &mut HashSet<String>,
) {
    let is_busy = |checkout: &Path| lock(view).is_busy(checkout);
    let worktrees = match prune::merged_worktrees(main_checkout, is_busy) {
        Ok(worktrees) => worktrees,
        Err(error) => {
            errors.insert(format!(
                "prune_merged {}: {}",
                main_checkout.display(),
                one_line(&error)
            ));
            return;
        }
    };
    for worktree in worktrees {
        if is_busy(&worktree.path) {
            continue;
        }
        let current_commit = git::branch_commit(main_checkout, &worktree.branch);
        if !worktree.is_unchanged(current_commit.as_deref()) {
            continue;
        }
        match wt::remove_merged(main_checkout, &worktree.path) {
            Ok(()) => log.write(&format!(
                "removed the worktree of {}: its pull request is merged",
                worktree.branch
            )),
            Err(error) => {
                errors.insert(format!(
                    "wt remove {}: {}",
                    worktree.branch,
                    one_line(&error)
                ));
            }
        }
    }
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
/// file, then rename it: the observer never reads a partial path.
fn write_wanted_binary(state_dir: &Path, binary: &Path) -> Result<(), Error> {
    let path = wanted_binary_path(state_dir);
    let temporary = state_dir.join("observer.binary.tmp");
    fs::write(&temporary, binary.as_os_str().as_bytes())
        .and_then(|()| fs::rename(&temporary, &path))
        .map_err(|error| Error::Command(format!("cannot write {}: {error}", path.display())))
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
    written
}

/// The modification time of the binary of the observer, or None if the
/// binary does not exist.
fn binary_time() -> Option<SystemTime> {
    let executable = env::current_exe().ok()?;
    fs::metadata(executable).ok()?.modified().ok()
}

/// Run the observer until herdr stops.
///
/// If another observer has the lock, return at once. The observer runs
/// the binary of the last `observe` command, or its own binary again if
/// this binary changed (see `binary_to_run`). With `prune_merged`, the
/// observer also starts the thread of the merged pull requests.
pub fn run(state_dir: &Path, prune_merged: bool) -> Result<(), Error> {
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
    };
    observer.log.write("observer started");
    if let Some(view) = prune_view {
        observer.log.write("prune_merged: on");
        start_prune_thread(view, log);
    }
    let current_binary = canonical(&env::current_exe().unwrap_or_default());
    let started_time = binary_time();
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
        // A path to a binary that does not exist does not stop the observer.
        let wanted = read_wanted_binary(state_dir)
            .map(|path| canonical(&path))
            .filter(|path| path.is_file());
        let is_rebuilt = binary_time().is_some_and(|time| Some(time) != started_time);
        if let Some(binary) = binary_to_run(&current_binary, wanted.as_deref(), is_rebuilt) {
            observer.log.write(&format!(
                "new binary: {} -> {}",
                current_binary.display(),
                binary.display()
            ));
            // The new process takes the lock again: release it first.
            drop(lock);
            let error = Command::new(&binary).args(["observe", "--run"]).exec();
            return Err(Error::Command(format!(
                "cannot run the new binary {}: {error}",
                binary.display()
            )));
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
    fn an_agent_makes_busy_its_checkout_and_the_checkout_of_its_workspace() {
        let workspaces =
            HashMap::from([("w1".to_owned(), first_checkout(Some("/work/repo.feature")))]);
        let panes = [agent_in("w1", Some(checkout("/work/repo.other")))];

        let busy = busy_checkouts(&panes, &workspaces);

        assert_eq!(
            busy,
            HashSet::from([
                PathBuf::from("/work/repo.feature"),
                PathBuf::from("/work/repo.other"),
            ])
        );
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
    fn a_missing_or_empty_file_gives_no_wanted_binary() {
        let folder = test_folder("missing");

        assert_eq!(read_wanted_binary(&folder), None);
        fs::write(wanted_binary_path(&folder), "").unwrap();
        assert_eq!(read_wanted_binary(&folder), None);
        fs::remove_dir_all(&folder).unwrap();
    }
}
