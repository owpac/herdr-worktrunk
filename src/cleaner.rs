//! The popup cleaner: all the linked worktrees of the repositories of the
//! herdr workspaces, in fzf. The list shows at once. Threads check each
//! worktree in the background (local criteria, then gh), and its row
//! shows a spinner until its check ends. The leftovers go to the group
//! "To delete" at the top. The other worktrees stay in a group for each
//! project, with the reason that keeps them.
//!
//! A leftover has no agent, no change that is not committed, and:
//!
//! - a finished pull request (merged or closed) that contains its commit,
//! - or no pull request, and no activity for `stale_days` days.
//!
//! `Enter` deletes one worktree after a question, `ctrl-a` deletes the
//! group "To delete". Then the list comes back.
//!
//! fzf runs this binary for its events: `cleaner-rows` gives the rows,
//! `cleaner-move` moves the cursor over the titles of the groups, and
//! keeps it on its worktree when the list reloads,
//! `cleaner-enter` and `cleaner-delete-all` accept only a valid choice,
//! and `cleaner-preview` gives the preview. The checks send a reload to
//! fzf through its socket (`--listen`).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::fzf;
use crate::git::{self, canonical};
use crate::github::{self, PullRequestState};
use crate::herdr::{self, Error};
use crate::picker::{self, shell_quote, FZF_CANCEL_EXIT_CODE, FZF_NO_MATCH_EXIT_CODE};
use crate::prune::{self, BranchStatus, KeptWhy};
use crate::stale::{self, OldWorktree, DAY};
use crate::style::{
    self, part, ACCENT, BOLD, DIM, GREEN, MUTED, RED, REPOSITORY, RESET, RULE, YELLOW,
};

const DELETE_ALL_REQUEST: &str = "delete-all";
const GHOST: &str = "worktree — type to filter";
// The footer has the width of the list: the hints must be short.
const HINTS: [(&str, &str); 4] = [
    ("⏎", "delete"),
    ("^a", "delete all"),
    ("⇥", "preview"),
    ("esc", "close"),
];
/// The tabs of the preview: the 3 tabs of the picker, then the checklist.
const PREVIEW_TABS: [&str; 4] = ["diff", "log", "changes", "checks"];
const CHECKS_TAB: usize = 3;
/// The frames of the spinner of a row in check.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The fields of a row before its frames: id, branch, main checkout, kind.
const ROW_FIELDS: usize = 4;
/// The seconds between 2 frames of the spinner.
const FRAME_INTERVAL: &str = "0.1";
/// The time between 2 reloads of the list, while the checks end.
const RELOAD_INTERVAL: Duration = Duration::from_millis(100);
/// The attempts to reach the socket of fzf, one for each reload interval.
const SOCKET_ATTEMPTS: u32 = 30;
/// The checks that run at the same time: each one calls gh.
const WORKERS: usize = 8;
/// Set when the popup closes without a choice: the checks stop writing
/// the state file, so the plugin can remove it.
static CHECKS_STOPPED: AtomicBool = AtomicBool::new(false);
/// The threads that list the checkouts of the workspaces.
const LIST_THREADS: usize = 4;
/// The id of the title of the group "To delete".
const TO_DELETE_ID: &str = "group:to-delete";
const TITLE_KIND: &str = "title";
const WORKTREE_KIND: &str = "worktree";

/// Why the cleaner deletes or keeps a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The check runs.
    Checking,
    /// A leftover: its merged pull request contains its commit.
    Merged,
    /// A leftover: its closed pull request contains its commit.
    Closed,
    /// A leftover: no pull request, and no activity for `stale_days` days.
    Old,
    /// Kept: an open pull request.
    Open,
    /// Kept: no pull request, but some activity in the last days.
    Recent,
    /// Kept: changes that are not committed.
    Changes,
    /// Kept: an agent works in the worktree.
    Agent,
    /// Kept: the branch has work after its finished pull request.
    WorkAfter,
    /// Kept: a finished pull request, and the default branch contains the
    /// commit of the branch (a new worktree, or a merge of wt).
    InDefault,
    /// Kept: the check failed (gh, no remote on GitHub...).
    Unknown,
}

const REASON_CODES: [(Reason, &str); 11] = [
    (Reason::Checking, "checking"),
    (Reason::Merged, "merged"),
    (Reason::Closed, "closed"),
    (Reason::Old, "old"),
    (Reason::Open, "open"),
    (Reason::Recent, "recent"),
    (Reason::Changes, "changes"),
    (Reason::Agent, "agent"),
    (Reason::WorkAfter, "work-after"),
    (Reason::InDefault, "in-default"),
    (Reason::Unknown, "unknown"),
];

impl Reason {
    fn code(self) -> &'static str {
        REASON_CODES
            .iter()
            .find(|(reason, _)| *reason == self)
            .map(|(_, code)| *code)
            .unwrap_or("unknown")
    }

    fn from_code(code: &str) -> Option<Reason> {
        REASON_CODES
            .iter()
            .find(|(_, known)| *known == code)
            .map(|(reason, _)| *reason)
    }

    /// Return true for a leftover: the group "To delete".
    pub fn is_leftover(self) -> bool {
        matches!(self, Reason::Merged | Reason::Closed | Reason::Old)
    }

    /// The glyph of a finished check, and its color.
    fn glyph(self) -> (&'static str, &'static str) {
        match self {
            Reason::Checking => (ACCENT, SPINNER[0]),
            Reason::Merged | Reason::Closed | Reason::Old => (RED, "✕"),
            Reason::Open | Reason::Recent | Reason::WorkAfter | Reason::InDefault => (GREEN, "✓"),
            Reason::Changes => (YELLOW, "●"),
            Reason::Agent => (ACCENT, "◐"),
            Reason::Unknown => (DIM, "?"),
        }
    }
}

/// A worktree of the list, and the result of its check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub branch: String,
    pub main_checkout: PathBuf,
    pub reason: Reason,
    /// The days without activity, once the check knows them.
    pub days: Option<u64>,
    /// The number of the pull request of the reason.
    pub pull_request: Option<u64>,
    /// The error of a check that failed.
    pub detail: String,
}

impl Entry {
    fn new(path: PathBuf, branch: String, main_checkout: PathBuf, reason: Reason) -> Entry {
        Entry {
            path,
            branch,
            main_checkout,
            reason,
            days: None,
            pull_request: None,
            detail: String::new(),
        }
    }

    fn number(&self) -> String {
        self.pull_request
            .map(|number| format!("#{number}"))
            .unwrap_or_default()
    }

    /// The reason in the row.
    fn reason_text(&self) -> String {
        let number = self.number();
        match self.reason {
            Reason::Checking => "checking…".to_owned(),
            Reason::Merged => format!("PR {number} merged"),
            Reason::Closed => format!("PR {number} closed"),
            Reason::Old => "no PR, no recent activity".to_owned(),
            Reason::Open => format!("PR {number} open"),
            Reason::Recent => "no PR, recent activity".to_owned(),
            Reason::Changes => "changes not committed".to_owned(),
            Reason::Agent => "an agent works here".to_owned(),
            Reason::WorkAfter => format!("work after PR {number}"),
            Reason::InDefault => format!("PR {number} finished, commit in the default branch"),
            Reason::Unknown => format!("not checked: {}", self.detail),
        }
    }

    /// The reason in the question of the popup confirm.
    pub fn question(&self) -> String {
        let number = self.number();
        let days = self.days.unwrap_or(0);
        match self.reason {
            Reason::Merged => format!("Its pull request {number} is merged."),
            Reason::Closed => format!("Its pull request {number} is closed."),
            Reason::Old => format!("No pull request, and no change for {days} days."),
            Reason::Open => format!("Careful: its pull request {number} is open."),
            Reason::Recent => format!("No pull request, but a change {days} days ago."),
            Reason::Changes => "Careful: it has changes that are not committed.".to_owned(),
            Reason::Agent => "Careful: an agent works in it.".to_owned(),
            Reason::WorkAfter => format!("Careful: the branch has work after {number}."),
            Reason::InDefault => format!(
                "Its pull request {number} is finished, and the default branch contains its commit."
            ),
            Reason::Checking | Reason::Unknown => "The cleaner did not check it.".to_owned(),
        }
    }

    fn as_old_worktree(&self) -> OldWorktree {
        OldWorktree {
            main_checkout: self.main_checkout.clone(),
            path: self.path.clone(),
            branch: self.branch.clone(),
            days: self.days.unwrap_or(0),
        }
    }
}

/// A line of the state file: the fields of an entry, separated by tabs.
fn record(entry: &Entry) -> String {
    let clean = |text: &str| text.replace(['\t', '\n'], " ");
    [
        entry.path.display().to_string(),
        clean(&entry.branch),
        entry.main_checkout.display().to_string(),
        entry.reason.code().to_owned(),
        entry
            .pull_request
            .map(|number| number.to_string())
            .unwrap_or_default(),
        entry.days.map(|days| days.to_string()).unwrap_or_default(),
        clean(&entry.detail),
    ]
    .join("\t")
}

fn parse_record(line: &str) -> Option<Entry> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [path, branch, main_checkout, code, pull_request, days, detail] = fields[..] else {
        return None;
    };
    Some(Entry {
        path: PathBuf::from(path),
        branch: branch.to_owned(),
        main_checkout: PathBuf::from(main_checkout),
        reason: Reason::from_code(code)?,
        days: days.parse().ok(),
        pull_request: pull_request.parse().ok(),
        detail: detail.to_owned(),
    })
}

/// Write the state file in one rename: fzf reads it at the same time.
pub fn write_state(path: &Path, entries: &[Entry]) -> io::Result<()> {
    let text: String = entries.iter().map(|entry| record(entry) + "\n").collect();
    stale::write_atomically(path, &text)
}

pub fn read_state(path: &Path) -> Vec<Entry> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(parse_record)
        .collect()
}

/// The main checkouts of the repositories of the herdr workspaces, and
/// the checkouts that an agent uses (its folder, or its workspace).
fn herdr_repositories() -> Result<(Vec<PathBuf>, HashSet<PathBuf>), Error> {
    let snapshot = herdr::snapshot()?;
    // A few threads: herdr and git take about 20 ms for each workspace,
    // and the list must show at once. herdr refuses too many worktree
    // checks at the same time.
    let next = Mutex::new(0_usize);
    let checkouts: Vec<(String, Option<(PathBuf, PathBuf)>)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..LIST_THREADS)
            .map(|_| {
                scope.spawn(|| {
                    let mut found = Vec::new();
                    loop {
                        let index = {
                            let Ok(mut next) = next.lock() else {
                                return found;
                            };
                            *next += 1;
                            *next - 1
                        };
                        let Some(workspace) = snapshot.workspaces.get(index) else {
                            return found;
                        };
                        let checkout = herdr::workspace_checkout(&workspace.workspace_id)
                            .ok()
                            .flatten()
                            .and_then(|checkout| {
                                let main =
                                    git::checkout_of(&checkout.checkout_path)?.main_checkout();
                                Some((canonical(&checkout.checkout_path), canonical(&main)))
                            });
                        found.push((workspace.workspace_id.clone(), checkout));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .flatten()
            .collect()
    });
    let mut repositories = Vec::new();
    let mut workspace_roots = HashMap::new();
    for (workspace_id, found) in checkouts {
        let Some((root, main)) = found else {
            continue;
        };
        workspace_roots.insert(workspace_id, root);
        if !repositories.contains(&main) {
            repositories.push(main);
        }
    }
    let mut busy = HashSet::new();
    for pane in snapshot.panes.iter().filter(|pane| pane.agent.is_some()) {
        if let Some(found) = pane.folder().and_then(git::checkout_of) {
            busy.insert(canonical(&found.root));
        }
        if let Some(root) = workspace_roots.get(&pane.workspace_id) {
            busy.insert(root.clone());
        }
    }
    Ok((repositories, busy))
}

/// The linked worktrees of `scope` (one repository), or of all the
/// repositories of the herdr workspaces. Only local data: no gh. A
/// worktree with an agent gets its reason at once. `previous` gives the
/// results of an earlier list (the return after a question): these
/// worktrees keep their result without a new check.
pub fn list_worktrees(scope: Option<&Path>, previous: &[Entry]) -> Result<Vec<Entry>, Error> {
    let (mut repositories, busy) = herdr_repositories()?;
    if let Some(scope) = scope {
        let main = git::checkout_of(scope)
            .map(|checkout| canonical(&checkout.main_checkout()))
            .ok_or_else(|| {
                Error::Command(format!("{} is not in a git repository", scope.display()))
            })?;
        repositories = vec![main];
    }
    let known: HashMap<&Path, &Entry> = previous
        .iter()
        .filter(|entry| {
            !matches!(
                entry.reason,
                Reason::Checking | Reason::Agent | Reason::Unknown
            )
        })
        .map(|entry| (entry.path.as_path(), entry))
        .collect();
    let listed: Vec<(PathBuf, Vec<git::LinkedWorktree>)> = thread::scope(|scope| {
        let handles: Vec<_> = repositories
            .into_iter()
            .map(|repository| {
                scope.spawn(move || {
                    // A repository that git cannot read shows no worktree.
                    let worktrees = git::linked_worktrees(&repository).unwrap_or_default();
                    (repository, worktrees)
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    let mut entries = Vec::new();
    for (repository, worktrees) in listed {
        for worktree in worktrees {
            let path = canonical(&worktree.path);
            if busy.contains(&path) {
                entries.push(Entry::new(
                    path,
                    worktree.branch,
                    repository.clone(),
                    Reason::Agent,
                ));
                continue;
            }
            match known.get(path.as_path()) {
                Some(entry) if entry.branch == worktree.branch => entries.push((*entry).clone()),
                _ => entries.push(Entry::new(
                    path,
                    worktree.branch,
                    repository.clone(),
                    Reason::Checking,
                )),
            }
        }
    }
    Ok(entries)
}

/// What a repository gives to the checks of its worktrees.
struct RepositoryFacts {
    /// The repository on GitHub, or the reason why there is none.
    github: Result<String, String>,
    default_branch: Option<String>,
}

fn repository_facts(main_checkout: &Path) -> RepositoryFacts {
    RepositoryFacts {
        github: prune::github_repository(main_checkout).map_err(|error| error.to_string()),
        default_branch: git::default_branch(main_checkout),
    }
}

/// The result of a check, from its facts: the reason, the number of the
/// pull request, and the error text of a failed check.
fn classify(
    is_clean: Option<bool>,
    days: Option<u64>,
    status: Result<&BranchStatus, &str>,
    stale_days: u32,
) -> (Reason, Option<u64>, String) {
    match is_clean {
        None => return (Reason::Unknown, None, "git status failed".to_owned()),
        Some(false) => return (Reason::Changes, None, String::new()),
        Some(true) => {}
    }
    match status {
        Err(error) => (Reason::Unknown, None, error.to_owned()),
        Ok(BranchStatus::Open(number)) => (Reason::Open, Some(*number), String::new()),
        Ok(BranchStatus::Kept { number, why }) => match why {
            KeptWhy::WorkAfter => (Reason::WorkAfter, Some(*number), String::new()),
            KeptWhy::InDefaultBranch => (Reason::InDefault, Some(*number), String::new()),
            KeptWhy::NoLocalCommit => {
                (Reason::Unknown, Some(*number), "no local commit".to_owned())
            }
        },
        Ok(BranchStatus::Finished { pull_request, .. }) => {
            let reason = if pull_request.state == PullRequestState::Merged {
                Reason::Merged
            } else {
                Reason::Closed
            };
            (reason, Some(pull_request.number), String::new())
        }
        Ok(BranchStatus::NoPullRequest) => match days {
            Some(days) if days >= u64::from(stale_days) => (Reason::Old, None, String::new()),
            Some(_) => (Reason::Recent, None, String::new()),
            None => (Reason::Unknown, None, "no date of activity".to_owned()),
        },
    }
}

/// Check one worktree: the local criteria first, then gh.
fn check(entry: &Entry, facts: &RepositoryFacts, stale_days: u32) -> Entry {
    let mut result = entry.clone();
    let is_clean = git::clean_status(&entry.path);
    let now = stale::now();
    result.days = git::last_activity(&entry.path).map(|time| now.saturating_sub(time) / DAY);
    let status = if is_clean != Some(true) {
        Err("not needed".to_owned())
    } else {
        match &facts.github {
            Err(error) => Err(error.clone()),
            // A gh that does not answer gives "not checked" after 15 s: the
            // worktree stays, and the spinner stops.
            Ok(repository) => prune::branch_status(
                &entry.main_checkout,
                repository,
                facts.default_branch.as_deref(),
                &entry.branch,
                github::POPUP_TIME_LIMIT,
            )
            .map_err(|error| error.to_string().replace('\n', " ")),
        }
    };
    let (reason, pull_request, detail) = classify(
        is_clean,
        result.days,
        status.as_ref().map_err(String::as_str),
        stale_days,
    );
    result.reason = reason;
    result.pull_request = pull_request;
    result.detail = detail;
    result
}

/// Check the entries in check, in `WORKERS` threads, in the background.
/// After each result, the state file changes and fzf reloads the list.
/// At the end, the spinner stops. The threads stop with the process.
fn start_checks(
    entries: Vec<Entry>,
    state_path: PathBuf,
    socket: PathBuf,
    reload_action: String,
    stale_days: u32,
) {
    let todo: VecDeque<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.reason == Reason::Checking)
        .map(|(index, _)| index)
        .collect();
    if todo.is_empty() {
        return;
    }
    let repositories: HashSet<PathBuf> = todo
        .iter()
        .map(|index| entries[*index].main_checkout.clone())
        .collect();
    let entries = Arc::new(Mutex::new(entries));
    let todo = Arc::new(Mutex::new(todo));
    let (sender, receiver) = mpsc::channel::<()>();
    thread::spawn(move || {
        let facts: Arc<HashMap<PathBuf, RepositoryFacts>> = Arc::new(
            repositories
                .into_iter()
                .map(|repository| {
                    let facts = repository_facts(&repository);
                    (repository, facts)
                })
                .collect(),
        );
        for _ in 0..WORKERS {
            let (entries, todo, facts, sender, state_path) = (
                Arc::clone(&entries),
                Arc::clone(&todo),
                Arc::clone(&facts),
                sender.clone(),
                state_path.clone(),
            );
            thread::spawn(move || loop {
                let Some(index) = todo.lock().ok().and_then(|mut todo| todo.pop_front()) else {
                    return;
                };
                let entry = match entries.lock() {
                    Ok(entries) => entries[index].clone(),
                    Err(_) => return,
                };
                let Some(facts) = facts.get(&entry.main_checkout) else {
                    continue;
                };
                let result = check(&entry, facts, stale_days);
                if let Ok(mut entries) = entries.lock() {
                    if CHECKS_STOPPED.load(Ordering::SeqCst) {
                        return;
                    }
                    entries[index] = result;
                    let _ = write_state(&state_path, &entries);
                }
                let _ = sender.send(());
            });
        }
    });
    thread::spawn(move || notify_fzf(&receiver, &socket, &reload_action));
}

/// Stop the checks: they do not write the state file anymore. The threads
/// stop with the process.
pub fn stop_checks() {
    CHECKS_STOPPED.store(true, Ordering::SeqCst);
}

/// The file that keeps the id of the row of the cursor before a reload:
/// the cursor goes back to this row after the reload (event load). A
/// reload does not keep the cursor on its row, because the rows change
/// their place. The plugin does it without the tracking of fzf: this
/// tracking blocks the input during a reload.
fn cursor_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("cursor")
}

/// fzf event `cleaner-remember`: keep the id of the row of the cursor,
/// just before a reload. The id comes from fzf (`{1}`), so it is the row
/// that the user sees, also after a click.
pub fn remember_cursor(state_path: &Path, id: &str) {
    if !id.is_empty() {
        let _ = fs::write(cursor_path(state_path), id);
    }
    // The checklist shows the new results of the checks.
    if picker::read_tab(state_path, PREVIEW_TABS.len()) == CHECKS_TAB {
        println!("refresh-preview");
    }
}

/// The file that keeps the rows that fzf shows, in their order: one line
/// `<kind>\t<id>` for each row. `cleaner-move` counts the rows in it,
/// because the state file can be newer than the list of fzf.
fn shown_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("shown")
}

fn write_shown(state_path: &Path, rows: &[Row]) {
    let text: String = rows
        .iter()
        .map(|row| {
            let kind = match row {
                Row::Title { .. } => TITLE_KIND,
                Row::Worktree(_) => WORKTREE_KIND,
            };
            format!("{kind}\t{}\n", row_id(row))
        })
        .collect();
    let _ = stale::write_atomically(&shown_path(state_path), &text);
}

/// The rows that fzf shows: for each row, true for a title, and its id.
fn read_shown(state_path: &Path) -> Option<Vec<(bool, String)>> {
    let text = fs::read_to_string(shown_path(state_path)).ok()?;
    Some(
        text.lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(kind, id)| (kind == TITLE_KIND, id.to_owned()))
            .collect(),
    )
}

/// Reload the list of fzf after the results, at most once in each reload
/// interval. When the checks end, stop the spinner.
fn notify_fzf(receiver: &mpsc::Receiver<()>, socket: &Path, reload: &str) {
    loop {
        match receiver.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => {
                thread::sleep(RELOAD_INTERVAL);
                while receiver.try_recv().is_ok() {}
                post_with_retries(socket, reload);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // The checks end: the header of a refused ctrl-a is not true anymore.
    let last = format!("{reload}+unbind(every({FRAME_INTERVAL}))+change-header:");
    post_with_retries(socket, &last);
}

/// Send an action to fzf. fzf can start after the first results: try
/// again for 3 seconds.
fn post_with_retries(socket: &Path, action: &str) {
    for _ in 0..SOCKET_ATTEMPTS {
        if fzf::post(socket, action).is_ok() {
            return;
        }
        thread::sleep(RELOAD_INTERVAL);
    }
}

/// A row of the list: the title of a group, or a worktree.
#[derive(Debug, PartialEq, Eq)]
enum Row<'a> {
    Title {
        id: String,
        label: String,
        count: usize,
        is_to_delete: bool,
    },
    Worktree(&'a Entry),
}

/// The rows in their order: the group "To delete" (the oldest first),
/// then one group for each project (by name, the worktrees by branch).
/// Without `with_titles` (a filter), the rows have no title.
fn layout(entries: &[Entry], with_titles: bool) -> Vec<Row<'_>> {
    let mut leftovers: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.reason.is_leftover())
        .collect();
    leftovers.sort_by(|a, b| b.days.cmp(&a.days).then_with(|| a.branch.cmp(&b.branch)));
    // One group for each repository, sorted by name, then by path.
    let mut projects: BTreeMap<(String, &Path), Vec<&Entry>> = BTreeMap::new();
    for entry in entries.iter().filter(|entry| !entry.reason.is_leftover()) {
        let name = git::folder_name(&entry.main_checkout);
        projects
            .entry((name, entry.main_checkout.as_path()))
            .or_default()
            .push(entry);
    }
    let mut name_count: HashMap<&str, usize> = HashMap::new();
    for (name, _) in projects.keys() {
        *name_count.entry(name.as_str()).or_default() += 1;
    }
    let labels: Vec<String> = projects
        .keys()
        .map(|(name, main_checkout)| {
            // 2 repositories with the same folder name show their path.
            if name_count.get(name.as_str()).copied().unwrap_or(0) > 1 {
                main_checkout.display().to_string()
            } else {
                name.clone()
            }
        })
        .collect();
    let mut rows = Vec::new();
    if !leftovers.is_empty() {
        if with_titles {
            rows.push(Row::Title {
                id: TO_DELETE_ID.to_owned(),
                label: "To delete".to_owned(),
                count: leftovers.len(),
                is_to_delete: true,
            });
        }
        rows.extend(leftovers.into_iter().map(Row::Worktree));
    }
    for (((_, main_checkout), mut group), label) in projects.into_iter().zip(labels) {
        group.sort_by(|a, b| a.branch.cmp(&b.branch));
        if with_titles {
            rows.push(Row::Title {
                id: format!("group:{}", main_checkout.display()),
                label,
                count: group.len(),
                is_to_delete: false,
            });
        }
        rows.extend(group.into_iter().map(Row::Worktree));
    }
    rows
}

/// The line of fzf for a row: the 4 hidden fields (id, branch, main
/// checkout, kind), then one text for each frame of the spinner. fzf
/// shows one frame at a time (`--with-nth`), and changes it every 0.1 s.
fn line(row: &Row, width: usize) -> String {
    let (fields, frames): ([String; ROW_FIELDS], Vec<String>) = match row {
        Row::Title {
            id,
            label,
            count,
            is_to_delete,
        } => {
            let color = if *is_to_delete { RED } else { REPOSITORY };
            let text = format!("{BOLD}{color}{label}{RESET}{RULE} · {RESET}{DIM}{count}{RESET}");
            (
                [
                    id.clone(),
                    String::new(),
                    String::new(),
                    TITLE_KIND.to_owned(),
                ],
                vec![text; SPINNER.len()],
            )
        }
        Row::Worktree(entry) => {
            // A finished check has the same text in each frame.
            let frame_count = if entry.reason == Reason::Checking {
                SPINNER.len()
            } else {
                1
            };
            let mut frames: Vec<String> = (0..frame_count)
                .map(|frame| {
                    let (color, glyph) = if entry.reason == Reason::Checking {
                        (ACCENT, SPINNER[frame])
                    } else {
                        entry.reason.glyph()
                    };
                    let reason_color = if entry.reason.is_leftover() {
                        RED
                    } else {
                        MUTED
                    };
                    let left = [
                        part("", "  "),
                        part(color, format!("{glyph} ")),
                        part("", entry.branch.clone()),
                        part(RULE, " — "),
                        part(reason_color, entry.reason_text()),
                    ];
                    let mut right = vec![part(MUTED, git::folder_name(&entry.path))];
                    if let Some(days) = entry.days {
                        right.push(part(MUTED, format!("{days}d")));
                    }
                    style::row(&left, &right, width)
                })
                .collect();
            frames.resize(SPINNER.len(), frames[0].clone());
            (
                [
                    entry.path.display().to_string(),
                    entry.branch.clone(),
                    entry.main_checkout.display().to_string(),
                    WORKTREE_KIND.to_owned(),
                ],
                frames,
            )
        }
    };
    let mut all = fields.to_vec();
    all.extend(frames);
    all.join("\t")
}

/// fzf event `cleaner-rows`: print the rows of the state file, with the
/// width of the popup. With a filter (`query`), the rows have no title.
pub fn print_rows(state_path: &Path, query: &str) {
    let entries = read_state(state_path);
    let width = style::row_width(style::fzf_columns());
    let rows = layout(&entries, query.trim().is_empty());
    write_shown(state_path, &rows);
    let mut stdout = io::stdout().lock();
    for row in rows {
        // fzf closes the pipe when a new reload starts: stop without error.
        if writeln!(stdout, "{}", line(&row, width)).is_err() {
            return;
        }
    }
}

/// A move of the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
    /// Back to the worktree of the last move of the user, after a
    /// reload. Without a move of the user, the first worktree.
    Keep,
}

/// The index of the worktree row after `current` in the direction of the
/// move, over the titles. None if no worktree row is there.
fn target(is_title: &[bool], current: usize, direction: Move) -> Option<usize> {
    match direction {
        Move::Keep => is_title.iter().position(|title| !title),
        Move::Down => (current + 1..is_title.len()).find(|index| !is_title[*index]),
        Move::Up => (0..current.min(is_title.len()))
            .rev()
            .find(|index| !is_title[*index]),
    }
}

/// fzf event `cleaner-move`: print the fzf action that moves the cursor
/// to the next worktree row, over the titles of the groups, or back to
/// its worktree after a reload. `current_index` (`{n}`) and `current_id`
/// (`{1}`) give the row of the cursor: the id finds it also if the list
/// changed. With a filter, the list has no title: an arrow is a normal
/// move, and a reload keeps the place of fzf.
pub fn print_move(
    state_path: &Path,
    direction: Move,
    current_index: &str,
    current_id: &str,
    query: &str,
) {
    if !query.trim().is_empty() {
        match direction {
            Move::Up => println!("up"),
            Move::Down => println!("down"),
            Move::Keep => {}
        }
        return;
    }
    let shown = read_shown(state_path).unwrap_or_else(|| {
        layout(&read_state(state_path), true)
            .iter()
            .map(|row| (matches!(row, Row::Title { .. }), row_id(row)))
            .collect()
    });
    let (is_title, ids): (Vec<bool>, Vec<String>) = shown.into_iter().unzip();
    let index = match direction {
        Move::Keep => fs::read_to_string(cursor_path(state_path))
            .ok()
            .and_then(|id| ids.iter().position(|known| *known == id))
            .or_else(|| target(&is_title, 0, Move::Keep)),
        Move::Up | Move::Down => {
            let current = ids
                .iter()
                .position(|known| known == current_id)
                .or_else(|| current_index.parse().ok())
                .unwrap_or(0);
            target(&is_title, current, direction)
        }
    };
    if let Some(index) = index {
        // pos counts from 1.
        println!("pos({})", index + 1);
    }
}

/// The id of a row: field 1 of its line.
fn row_id(row: &Row) -> String {
    match row {
        Row::Title { id, .. } => id.clone(),
        Row::Worktree(entry) => entry.path.display().to_string(),
    }
}

/// fzf event `cleaner-enter`: accept only a worktree row.
pub fn print_enter(kind: &str) {
    if kind == WORKTREE_KIND {
        println!("accept");
    }
}

/// fzf event `cleaner-delete-all`: accept the deletion of the group "To
/// delete" if it has worktrees.
pub fn print_delete_all(state_path: &Path) {
    if read_state(state_path)
        .iter()
        .any(|entry| entry.reason.is_leftover())
    {
        println!("print({DELETE_ALL_REQUEST})+accept");
    } else if read_state(state_path)
        .iter()
        .any(|entry| entry.reason == Reason::Checking)
    {
        println!("change-header:The checks run: wait for the group To delete.");
    } else {
        println!("change-header:The group To delete is empty.");
    }
}

/// The result of a check in the checklist of a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Pass,
    Fail,
    /// The cleaner did not need this check.
    Skip,
    /// The check runs.
    Wait,
    /// The check failed: no result.
    Unknown,
}

impl Mark {
    fn glyph(self) -> (&'static str, &'static str) {
        match self {
            Mark::Pass => (GREEN, "✓"),
            Mark::Fail => (RED, "✗"),
            Mark::Skip => (DIM, "–"),
            Mark::Wait => (ACCENT, "…"),
            Mark::Unknown => (YELLOW, "?"),
        }
    }
}

/// The checklist of a worktree: the 4 checks of the cleaner, in their
/// order, with their results. The cleaner stops at the first check that
/// keeps the worktree: the next checks show as not needed.
fn checklist(entry: &Entry, stale_days: u32) -> [(Mark, String); 4] {
    let number = entry.number();
    let days = entry.days.unwrap_or(0);
    let not_needed = |what: &str| (Mark::Skip, format!("{what}: not needed."));
    let agent_pass = (Mark::Pass, "No agent works in it.".to_owned());
    let clean_pass = (Mark::Pass, "No change that is not committed.".to_owned());
    let no_pull_request = (Mark::Pass, "No pull request.".to_owned());
    // The check that failed, from the error text.
    let unknown_step = match entry.detail.as_str() {
        "git status failed" => 2,
        "no date of activity" => 4,
        _ => 3,
    };
    match entry.reason {
        Reason::Checking => [
            agent_pass,
            (Mark::Wait, "Changes: checking…".to_owned()),
            (Mark::Wait, "Pull requests: checking…".to_owned()),
            (Mark::Wait, "Activity: checking…".to_owned()),
        ],
        Reason::Agent => [
            (Mark::Fail, "An agent works in it.".to_owned()),
            not_needed("Changes"),
            not_needed("Pull requests"),
            not_needed("Activity"),
        ],
        Reason::Changes => [
            agent_pass,
            (
                Mark::Fail,
                "It has changes that are not committed.".to_owned(),
            ),
            not_needed("Pull requests"),
            not_needed("Activity"),
        ],
        Reason::Unknown => {
            let failed = (Mark::Unknown, format!("Not checked: {}.", entry.detail));
            match unknown_step {
                2 => [
                    agent_pass,
                    failed,
                    not_needed("Pull requests"),
                    not_needed("Activity"),
                ],
                4 => [agent_pass, clean_pass, no_pull_request, failed],
                _ => [agent_pass, clean_pass, failed, not_needed("Activity")],
            }
        }
        Reason::Open | Reason::WorkAfter | Reason::InDefault => {
            let text = match entry.reason {
                Reason::Open => format!("Pull request {number} is open."),
                Reason::WorkAfter => {
                    format!(
                        "Pull request {number} is finished, but the branch has commits after it."
                    )
                }
                _ => format!(
                    "Pull request {number} is finished, but the default branch contains the commit."
                ),
            };
            [
                agent_pass,
                clean_pass,
                (Mark::Fail, text),
                not_needed("Activity"),
            ]
        }
        Reason::Merged | Reason::Closed => {
            let state = if entry.reason == Reason::Merged {
                "merged"
            } else {
                "closed"
            };
            [
                agent_pass,
                clean_pass,
                (
                    Mark::Pass,
                    format!(
                        "Pull request {number} is {state}, with the last commit of the branch."
                    ),
                ),
                (
                    Mark::Skip,
                    "Activity: not needed after a finished pull request.".to_owned(),
                ),
            ]
        }
        Reason::Old => [
            agent_pass,
            clean_pass,
            no_pull_request,
            (
                Mark::Pass,
                format!("No activity for {days} days (limit: {stale_days} days)."),
            ),
        ],
        Reason::Recent => [
            agent_pass,
            clean_pass,
            no_pull_request,
            (
                Mark::Fail,
                format!("Activity {days} days ago (limit: {stale_days} days)."),
            ),
        ],
    }
}

/// The tab `checks`: the verdict, the checklist, and the rule of a
/// leftover.
fn print_checklist(entry: &Entry, stale_days: u32) {
    let verdict = match entry.reason {
        Reason::Checking => format!("{BOLD}{ACCENT}Checking…{RESET}"),
        reason if reason.is_leftover() => format!("{BOLD}{RED}To delete{RESET}"),
        _ => format!(
            "{BOLD}{GREEN}Kept{RESET}{RULE} · {RESET}{MUTED}{}{RESET}",
            entry.reason_text()
        ),
    };
    println!("{verdict}");
    println!();
    for (mark, text) in checklist(entry, stale_days) {
        let (color, glyph) = mark.glyph();
        let text_color = if mark == Mark::Skip { DIM } else { "" };
        println!("{color}{glyph}{RESET} {text_color}{text}{RESET}");
    }
    println!();
    // The rule of a leftover, on short lines: the preview does not wrap.
    for line in [
        "A worktree goes to the group To delete if:".to_owned(),
        "- it has no agent and no change,".to_owned(),
        "- and it has a finished pull request,".to_owned(),
        format!("- or no pull request and no activity for {stale_days} days."),
    ] {
        println!("{DIM}{line}{RESET}");
    }
}

/// fzf event `cleaner-preview`: the summary of a group, or the preview of
/// a worktree: the tabs of the picker, then the checklist (`checks`).
pub fn print_preview(
    state_path: &Path,
    stale_days: u32,
    id: &str,
    branch: &str,
    main_checkout: &str,
    kind: &str,
) {
    let entries = read_state(state_path);
    if kind == WORKTREE_KIND {
        let tab = picker::read_tab(state_path, PREVIEW_TABS.len());
        println!("{}", style::tabs(&PREVIEW_TABS, tab));
        println!("{}", style::rule(style::preview_columns()));
        if tab == CHECKS_TAB {
            match entries.iter().find(|entry| entry.path == Path::new(id)) {
                Some(entry) => print_checklist(entry, stale_days),
                None => println!("{MUTED}No result for this worktree.{RESET}"),
            }
        } else {
            picker::print_tab(Path::new(main_checkout), PREVIEW_TABS[tab], branch, id);
        }
        return;
    }
    let members = entries.iter().filter(|entry| {
        if id == TO_DELETE_ID {
            entry.reason.is_leftover()
        } else {
            !entry.reason.is_leftover() && format!("group:{}", entry.main_checkout.display()) == id
        }
    });
    for entry in members {
        let (color, glyph) = entry.reason.glyph();
        println!(
            "{color}{glyph}{RESET} {} {RULE}—{RESET} {MUTED}{}{RESET}",
            entry.branch,
            entry.reason_text()
        );
    }
}

/// The scope of the list, on the right of the prompt: its repository, or
/// the number of its repositories.
fn scope_of(entries: &[Entry]) -> String {
    let repositories: HashSet<&Path> = entries
        .iter()
        .map(|entry| entry.main_checkout.as_path())
        .collect();
    match repositories.iter().next() {
        Some(repository) if repositories.len() == 1 => git::folder_name(repository),
        _ => format!("{} repositories", repositories.len()),
    }
}

/// What the user chose in the list.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Delete(Entry),
    /// The worktrees of the group "To delete".
    DeleteAll(Vec<OldWorktree>),
}

/// Show the list in fzf, start the checks, and return the choice of the
/// user. `state_path` is the state file: the checks write their results
/// in it, and the preview keeps its tab next to it.
pub fn choose(
    entries: Vec<Entry>,
    state_path: &Path,
    stale_days: u32,
) -> Result<Option<Choice>, Error> {
    write_state(state_path, &entries)
        .map_err(|error| Error::Command(format!("cannot write the list: {error}")))?;
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let binary = shell_quote(&executable.to_string_lossy());
    let state = shell_quote(&state_path.to_string_lossy());
    let rows_command = format!("{binary} cleaner-rows {state} {{q}}");
    let keep_command = format!("{binary} cleaner-move {state} keep {{n}} {{1}} {{q}}");
    // Each reload first keeps the id of the row of the cursor.
    let reload_action =
        format!("transform({binary} cleaner-remember {state} {{1}})+reload({rows_command})");
    let move_command = |direction: &str| {
        format!("transform({binary} cleaner-move {state} {direction} {{n}} {{1}} {{q}})")
    };
    let socket = fzf::socket_path("herdr-worktrunk-cleaner");
    let columns = style::terminal_columns();
    let is_checking = entries.iter().any(|entry| entry.reason == Reason::Checking);
    let mut command = Command::new("fzf");
    command
        .env(style::COLUMNS_VARIABLE, columns.to_string())
        // With an API key, fzf refuses the actions of the checks.
        .env_remove("FZF_API_KEY")
        .args(style::fzf_arguments(GHOST, &scope_of(&entries), &HINTS))
        // The first frame of the spinner.
        .args(["--with-nth", &(ROW_FIELDS + 1).to_string()])
        .arg("--listen")
        .arg(&socket)
        .arg("--preview")
        .arg(format!(
            "{binary} cleaner-preview {state} {stale_days} {{1}} {{2}} {{3}} {{4}}"
        ));
    let mut binds = vec![
        format!("change:{reload_action}"),
        format!("resize:{reload_action}"),
        // After each load of the list (the first one, and each reload),
        // the cursor goes back to its worktree. A reload does not keep
        // the cursor on its row: the rows change their place.
        format!("load:transform({keep_command})"),
        format!("up:{}", move_command("up")),
        format!("down:{}", move_command("down")),
        format!("ctrl-p:{}", move_command("up")),
        format!("ctrl-n:{}", move_command("down")),
        format!("ctrl-k:{}", move_command("up")),
        format!("ctrl-j:{}", move_command("down")),
        // Enter and a double-click accept a worktree row only.
        format!("enter:transform({binary} cleaner-enter {{4}})"),
        format!("double-click:transform({binary} cleaner-enter {{4}})"),
        format!("ctrl-a:transform({binary} cleaner-delete-all {state})"),
        format!(
            "tab:transform({binary} picker-tab {state} next {})",
            PREVIEW_TABS.len()
        ),
        format!(
            "btab:transform({binary} picker-tab {state} previous {})",
            PREVIEW_TABS.len()
        ),
    ];
    if is_checking {
        // Rotate the frames of the spinner, without a process.
        let frames: Vec<String> = (1..SPINNER.len())
            .chain([0])
            .map(|frame| (ROW_FIELDS + 1 + frame).to_string())
            .collect();
        binds.push(format!(
            "every({FRAME_INTERVAL}):change-with-nth({})",
            frames.join("|")
        ));
    }
    for bind in &binds {
        command.arg("--bind").arg(bind);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Command(format!("cannot run fzf: {error}")))?;
    if let Some(mut input) = child.stdin.take() {
        let width = style::row_width(columns);
        let shown = layout(&entries, true);
        write_shown(state_path, &shown);
        let rows: String = shown.iter().map(|row| line(row, width) + "\n").collect();
        // fzf closes its input when the user quits: a failed write is not
        // an error.
        let _ = input.write_all(rows.as_bytes());
    }
    let _ = fs::remove_file(cursor_path(state_path));
    start_checks(
        entries,
        state_path.to_path_buf(),
        socket,
        reload_action,
        stale_days,
    );
    let output = child
        .wait_with_output()
        .map_err(|error| Error::Command(format!("fzf failed: {error}")))?;
    let _ = fs::remove_file(state_path.with_extension("tab"));
    let _ = fs::remove_file(cursor_path(state_path));
    let _ = fs::remove_file(shown_path(state_path));
    match output.status.code() {
        Some(0) => {}
        Some(FZF_CANCEL_EXIT_CODE) | Some(FZF_NO_MATCH_EXIT_CODE) => return Ok(None),
        _ => return Err(Error::Command(style::FZF_FAILED.to_owned())),
    }
    Ok(parse_choice(
        &String::from_utf8_lossy(&output.stdout),
        &read_state(state_path),
    ))
}

/// Read the output of fzf: `delete-all`, or the line of one worktree.
fn parse_choice(text: &str, entries: &[Entry]) -> Option<Choice> {
    let first = text.lines().next()?;
    if first == DELETE_ALL_REQUEST {
        let leftovers: Vec<OldWorktree> = entries
            .iter()
            .filter(|entry| entry.reason.is_leftover())
            .map(Entry::as_old_worktree)
            .collect();
        return (!leftovers.is_empty()).then_some(Choice::DeleteAll(leftovers));
    }
    let path = first.split('\t').next()?;
    entries
        .iter()
        .find(|entry| entry.path == Path::new(path))
        .cloned()
        .map(Choice::Delete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PullRequest;

    fn entry(branch: &str, project: &str, reason: Reason, days: Option<u64>) -> Entry {
        Entry {
            path: PathBuf::from(format!("/work/{project}.{branch}")),
            branch: branch.to_owned(),
            main_checkout: PathBuf::from(format!("/work/{project}")),
            reason,
            days,
            pull_request: None,
            detail: String::new(),
        }
    }

    fn entries() -> Vec<Entry> {
        vec![
            entry("b-open", "zeta", Reason::Open, Some(1)),
            entry("a-old", "alpha", Reason::Old, Some(12)),
            entry("c-check", "alpha", Reason::Checking, None),
            entry("d-merged", "zeta", Reason::Merged, Some(40)),
        ]
    }

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Title { id, .. } => id.clone(),
                Row::Worktree(entry) => entry.branch.clone(),
            })
            .collect()
    }

    #[test]
    fn the_leftovers_come_first_then_one_group_for_each_project() {
        let entries = entries();

        let rows = layout(&entries, true);

        assert_eq!(
            ids(&rows),
            [
                "group:to-delete",
                "d-merged",
                "a-old",
                "group:/work/alpha",
                "c-check",
                "group:/work/zeta",
                "b-open"
            ]
        );
    }

    #[test]
    fn a_filter_removes_the_titles() {
        let entries = entries();

        let rows = layout(&entries, false);

        assert_eq!(ids(&rows), ["d-merged", "a-old", "c-check", "b-open"]);
    }

    #[test]
    fn without_leftover_the_group_to_delete_goes_away() {
        let entries = vec![entry("b-open", "zeta", Reason::Open, Some(1))];

        assert_eq!(ids(&layout(&entries, true)), ["group:/work/zeta", "b-open"]);
    }

    #[test]
    fn a_row_has_the_hidden_fields_and_one_frame_for_each_spinner_glyph() {
        let checking = entry("c-check", "alpha", Reason::Checking, None);

        let text = line(&Row::Worktree(&checking), 60);
        let fields: Vec<&str> = text.split('\t').collect();

        assert_eq!(fields.len(), ROW_FIELDS + SPINNER.len());
        assert_eq!(
            fields[..ROW_FIELDS],
            ["/work/alpha.c-check", "c-check", "/work/alpha", "worktree"]
        );
        assert!(style::visible(fields[ROW_FIELDS]).contains("⠋ c-check — checking…"));
        assert!(style::visible(fields[ROW_FIELDS + 1]).contains("⠙ c-check"));
    }

    #[test]
    fn a_finished_check_has_the_same_text_in_each_frame() {
        let old = entry("a-old", "alpha", Reason::Old, Some(12));

        let text = line(&Row::Worktree(&old), 70);
        let frames: HashSet<&str> = text.split('\t').skip(ROW_FIELDS).collect();

        assert_eq!(frames.len(), 1);
        let shown = style::visible(frames.into_iter().next().unwrap());
        assert!(
            shown.contains("✕ a-old — no PR, no recent activity"),
            "{shown}"
        );
        assert!(shown.ends_with("alpha.a-old · 12d"), "{shown}");
    }

    #[test]
    fn two_repositories_with_the_same_name_get_two_groups() {
        let mut other = entry("b", "app", Reason::Open, Some(1));
        other.main_checkout = PathBuf::from("/other/app");
        other.path = PathBuf::from("/other/app.b");
        let entries = vec![entry("a", "app", Reason::Open, Some(1)), other];

        let rows = layout(&entries, true);

        let labels: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Title { label, .. } => Some(label.as_str()),
                Row::Worktree(_) => None,
            })
            .collect();
        assert_eq!(labels, ["/other/app", "/work/app"]);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn the_checklist_gives_each_check_in_its_order() {
        let marks = |entry: &Entry| -> Vec<Mark> {
            checklist(entry, 7).iter().map(|(mark, _)| *mark).collect()
        };
        let mut merged = entry("a", "alpha", Reason::Merged, Some(1));
        merged.pull_request = Some(12);
        let mut failed = entry("b", "alpha", Reason::Unknown, None);
        failed.detail = "git status failed".to_owned();

        assert_eq!(
            marks(&entry("a", "alpha", Reason::Old, Some(9))),
            [Mark::Pass, Mark::Pass, Mark::Pass, Mark::Pass]
        );
        assert_eq!(
            marks(&entry("a", "alpha", Reason::Recent, Some(2))),
            [Mark::Pass, Mark::Pass, Mark::Pass, Mark::Fail]
        );
        assert_eq!(
            marks(&merged),
            [Mark::Pass, Mark::Pass, Mark::Pass, Mark::Skip]
        );
        assert_eq!(
            marks(&entry("a", "alpha", Reason::Agent, None)),
            [Mark::Fail, Mark::Skip, Mark::Skip, Mark::Skip]
        );
        assert_eq!(
            marks(&failed),
            [Mark::Pass, Mark::Unknown, Mark::Skip, Mark::Skip]
        );
        assert!(checklist(&merged, 7)[2].1.contains("#12 is merged"));
        assert!(
            checklist(&entry("a", "alpha", Reason::Recent, Some(2)), 7)[3]
                .1
                .contains("2 days ago (limit: 7 days)")
        );
    }

    /// Run git in `folder` for a test, without the global config (for
    /// example a signature with an agent).
    fn git_in(folder: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(folder)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    #[test]
    fn a_repository_without_github_keeps_its_worktree_without_gh() {
        let base = env::temp_dir().join(format!("herdr-worktrunk-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let main = base.join("repo");
        fs::create_dir_all(&main).unwrap();
        git_in(&main, &["init", "-q", "-b", "main"]);
        git_in(&main, &["commit", "-q", "--allow-empty", "-m", "start"]);
        let worktree = base.join("repo.old");
        git_in(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "old",
                worktree.to_str().unwrap(),
            ],
        );
        let entry = Entry::new(
            worktree.clone(),
            "old".to_owned(),
            main.clone(),
            Reason::Checking,
        );

        // No remote origin: the check does not call gh, and keeps the
        // worktree with the reason.
        let result = check(&entry, &repository_facts(&main), 0);

        assert_eq!(result.reason, Reason::Unknown);
        assert_eq!(result.detail, "no remote origin");
        assert!(!result.reason.is_leftover());
        assert_eq!(checklist(&result, 0)[2].0, Mark::Unknown);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_failed_call_to_gh_never_gives_a_leftover() {
        for days in [Some(0), Some(400), None] {
            let (reason, _, detail) =
                classify(Some(true), days, Err("gh pr: no end after 15 s"), 7);

            assert_eq!(reason, Reason::Unknown);
            assert!(!reason.is_leftover());
            assert_eq!(detail, "gh pr: no end after 15 s");
        }
    }

    #[test]
    fn the_cursor_skips_the_titles() {
        let is_title = [true, false, false, true, false];

        assert_eq!(target(&is_title, 0, Move::Keep), Some(1));
        assert_eq!(target(&is_title, 2, Move::Down), Some(4));
        assert_eq!(target(&is_title, 4, Move::Up), Some(2));
        assert_eq!(target(&is_title, 1, Move::Up), None);
        assert_eq!(target(&is_title, 4, Move::Down), None);
    }

    #[test]
    fn the_state_file_keeps_each_entry() {
        let mut first = entry("feat/a", "alpha", Reason::Merged, Some(3));
        first.pull_request = Some(12);
        let mut second = entry("b", "alpha", Reason::Unknown, None);
        second.detail = "gh failed".to_owned();

        assert_eq!(parse_record(&record(&first)), Some(first));
        assert_eq!(parse_record(&record(&second)), Some(second));
        assert_eq!(parse_record("a\tb"), None);
    }

    fn finished(state: PullRequestState) -> BranchStatus {
        BranchStatus::Finished {
            pull_request: PullRequest {
                number: 7,
                state,
                head: "abc".to_owned(),
            },
            commit: "abc".to_owned(),
        }
    }

    /// A case of `classify`: clean, days, status of the branch, reason.
    type Case<'a> = (
        Option<bool>,
        Option<u64>,
        Result<&'a BranchStatus, &'a str>,
        Reason,
    );

    #[test]
    fn classify_gives_the_reason_of_each_case() {
        let none = BranchStatus::NoPullRequest;
        let merged = finished(PullRequestState::Merged);
        let closed = finished(PullRequestState::Closed);
        let open = BranchStatus::Open(3);
        let kept = BranchStatus::Kept {
            number: 3,
            why: KeptWhy::WorkAfter,
        };
        let in_default = BranchStatus::Kept {
            number: 3,
            why: KeptWhy::InDefaultBranch,
        };
        let cases: [Case; 11] = [
            (Some(false), Some(30), Ok(&none), Reason::Changes),
            (None, Some(30), Ok(&none), Reason::Unknown),
            (Some(true), Some(30), Err("gh failed"), Reason::Unknown),
            (Some(true), Some(1), Ok(&open), Reason::Open),
            (Some(true), Some(1), Ok(&kept), Reason::WorkAfter),
            (Some(true), Some(1), Ok(&in_default), Reason::InDefault),
            (Some(true), Some(1), Ok(&merged), Reason::Merged),
            (Some(true), Some(1), Ok(&closed), Reason::Closed),
            (Some(true), Some(7), Ok(&none), Reason::Old),
            (Some(true), Some(6), Ok(&none), Reason::Recent),
            (Some(true), None, Ok(&none), Reason::Unknown),
        ];
        for (is_clean, days, status, expected) in cases {
            let (reason, _, _) = classify(is_clean, days, status, 7);
            assert_eq!(reason, expected, "{is_clean:?} {days:?} {status:?}");
        }
    }

    #[test]
    fn the_scope_is_the_repository_or_the_count_of_repositories() {
        let mut list = vec![entry("a", "alpha", Reason::Old, Some(9))];
        assert_eq!(scope_of(&list), "alpha");

        list.push(entry("b", "zeta", Reason::Old, Some(9)));
        assert_eq!(scope_of(&list), "2 repositories");
    }

    #[test]
    fn delete_all_takes_the_leftovers_only() {
        let entries = entries();

        let choice = parse_choice("delete-all\n", &entries);

        let Some(Choice::DeleteAll(old)) = choice else {
            panic!("no choice");
        };
        let branches: Vec<&str> = old
            .iter()
            .map(|worktree| worktree.branch.as_str())
            .collect();
        assert_eq!(branches, ["a-old", "d-merged"]);
    }

    #[test]
    fn the_output_of_fzf_gives_the_worktree() {
        let entries = entries();
        let text = line(&Row::Worktree(&entries[0]), 60);

        assert_eq!(
            parse_choice(&format!("{text}\n"), &entries),
            Some(Choice::Delete(entries[0].clone()))
        );
        assert_eq!(parse_choice("", &entries), None);
    }
}
