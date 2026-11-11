//! The worktrees that the plugin removed, and their restore (action
//! restore).
//!
//! Before each removal (cleaner, action remove, observer), the plugin
//! records the worktree in `removed.jsonl` of the state folder: its
//! repository, its folder, its branch, the commit of its branch, and its
//! agents with their sessions. The 20 last removals stay. The action
//! restore lists them (see `restorer`), and brings back the chosen one:
//!
//! 1. It creates the branch again on its commit, if the branch is gone.
//! 2. `wt switch` creates the worktree again. The hooks of wt open its
//!    herdr workspace.
//! 3. It resumes each agent in a new tab of this workspace:
//!    `claude --resume <session>` or `codex resume <session>`.
//!
//! A program that ran in a pane, and a file that an editor did not save,
//! do not come back. The observer keeps a restored worktree for 1 day,
//! while its branch has the restored commit (`restored.jsonl`): else it
//! removes a finished worktree again at once.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::stale;

/// The number of removals that the file keeps.
const MAX_REMOVALS: usize = 20;

/// An agent of a removed worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedAgent {
    /// The kind of the agent: claude, codex...
    pub kind: String,
    pub session: String,
}

impl RemovedAgent {
    /// The agent of `kind` with `session`, or None without a session: it
    /// cannot come back.
    pub fn of(kind: &str, session: Option<&str>) -> Option<RemovedAgent> {
        Some(RemovedAgent {
            kind: kind.to_owned(),
            session: session.filter(|session| !session.is_empty())?.to_owned(),
        })
    }

    /// The arguments that resume the conversation, after the name of the
    /// agent, or None for a kind that the plugin cannot resume.
    pub fn resume_arguments(&self) -> Option<Vec<String>> {
        match self.kind.as_str() {
            "claude" => Some(vec!["--resume".to_owned(), self.session.clone()]),
            "codex" => Some(vec!["resume".to_owned(), self.session.clone()]),
            _ => None,
        }
    }

    /// A herdr name for the resumed agent: `[a-z][a-z0-9_-]`, 32
    /// characters or less.
    pub fn herdr_name(&self) -> String {
        let session: String = self
            .session
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|character| character.to_ascii_lowercase())
            .take(8)
            .collect();
        format!("restored-{session}")
    }
}

/// A removed worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    /// The time of the removal, in seconds since 1970.
    pub time: u64,
    pub main_checkout: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// The commit of the branch at the removal.
    pub commit: String,
    pub agents: Vec<RemovedAgent>,
    /// The command that gets the branch back from the forge, for a closed
    /// pull request.
    #[serde(default)]
    pub recover_command: Option<String>,
}

fn removed_path(state_dir: &Path) -> PathBuf {
    state_dir.join("removed.jsonl")
}

fn restored_path(state_dir: &Path) -> PathBuf {
    state_dir.join("restored.jsonl")
}

/// Read the lines of a JSON lines file. A line that is not valid stays
/// out.
fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn write_lines<T: Serialize>(path: &Path, items: &[T]) {
    let text: String = items
        .iter()
        .filter_map(|item| serde_json::to_string(item).ok())
        .map(|line| format!("{line}\n"))
        .collect();
    // A failed write loses only the restore of a removal.
    let _ = stale::write_atomically(path, &text);
}

/// Record a removal, before it starts: its agents are still there. A new
/// record of the same worktree replaces the older one: a removal that
/// failed comes again at the next pass of the observer.
pub fn record(state_dir: &Path, removal: &Removal) {
    let mut removals: Vec<Removal> = read_lines(&removed_path(state_dir));
    removals.retain(|other| other.path != removal.path);
    removals.push(removal.clone());
    let excess = removals.len().saturating_sub(MAX_REMOVALS);
    write_lines(&removed_path(state_dir), &removals[excess..]);
}

/// All the removals of the file, the oldest first.
pub fn all(state_dir: &Path) -> Vec<Removal> {
    read_lines(&removed_path(state_dir))
}

/// Remove `removal` from the file: it came back, or its worktree is there.
pub fn forget(state_dir: &Path, removal: &Removal) {
    let removals: Vec<Removal> = read_lines(&removed_path(state_dir));
    let kept: Vec<Removal> = removals
        .into_iter()
        .filter(|other| other != removal)
        .collect();
    write_lines(&removed_path(state_dir), &kept);
}

/// A restored worktree, that the observer keeps for 1 day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Restored {
    path: PathBuf,
    commit: String,
    time: u64,
}

/// Record a restored worktree, with the commit of its branch.
pub fn mark_restored(state_dir: &Path, path: &Path, commit: &str) {
    let now = stale::now();
    let mut restored: Vec<Restored> = read_lines(&restored_path(state_dir));
    restored.retain(|item| item.path != path && now.saturating_sub(item.time) < stale::DAY);
    restored.push(Restored {
        path: path.to_owned(),
        commit: commit.to_owned(),
        time: now,
    });
    write_lines(&restored_path(state_dir), &restored);
}

/// Return true if the observer must keep the worktree `path`: a restore of
/// less than 1 day, and its branch still has the restored commit.
pub fn is_kept(state_dir: &Path, path: &Path, commit: &str) -> bool {
    let now = stale::now();
    read_lines::<Restored>(&restored_path(state_dir))
        .iter()
        .any(|item| {
            item.path == path && item.commit == commit && now.saturating_sub(item.time) < stale::DAY
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_folder;

    fn removal(branch: &str, agents: Vec<RemovedAgent>) -> Removal {
        Removal {
            time: 10,
            main_checkout: PathBuf::from("/work/app"),
            path: PathBuf::from(format!("/work/app.{branch}")),
            branch: branch.to_owned(),
            commit: "abc123".to_owned(),
            agents,
            recover_command: None,
        }
    }

    #[test]
    fn the_last_removal_comes_first_and_only_20_stay() {
        let folder = test_folder("removals");
        for index in 0..25 {
            record(&folder, &removal(&format!("b{index}"), vec![]));
        }

        let last_one = all(&folder).pop().unwrap();
        assert_eq!(last_one.branch, "b24");
        assert_eq!(read_lines::<Removal>(&removed_path(&folder)).len(), 20);

        forget(&folder, &last_one);
        assert_eq!(all(&folder).pop().unwrap().branch, "b23");
    }

    #[test]
    fn a_new_record_of_the_same_worktree_replaces_the_older_one() {
        let folder = test_folder("removal-again");
        record(&folder, &removal("feat", vec![]));
        record(&folder, &removal("other", vec![]));
        let mut again = removal("feat", vec![]);
        again.time = 20;
        record(&folder, &again);

        let removals: Vec<Removal> = read_lines(&removed_path(&folder));
        let branches: Vec<&str> = removals
            .iter()
            .map(|removal| removal.branch.as_str())
            .collect();
        assert_eq!(branches, ["other", "feat"]);
        assert_eq!(removals[1].time, 20);
    }

    #[test]
    fn an_agent_without_session_cannot_come_back() {
        assert_eq!(RemovedAgent::of("claude", None), None);
        assert_eq!(RemovedAgent::of("claude", Some("")), None);
        let claude = RemovedAgent::of("claude", Some("A1b2-C3d4-e5")).unwrap();
        let codex = RemovedAgent::of("codex", Some("019a")).unwrap();

        assert_eq!(
            claude.resume_arguments().unwrap(),
            ["--resume", "A1b2-C3d4-e5"]
        );
        assert_eq!(codex.resume_arguments().unwrap(), ["resume", "019a"]);
        assert_eq!(
            RemovedAgent::of("pi", Some("s"))
                .unwrap()
                .resume_arguments(),
            None
        );
        assert_eq!(claude.herdr_name(), "restored-a1b2c3d4");
    }

    #[test]
    fn the_observer_keeps_a_restored_worktree_while_its_commit_stays() {
        let folder = test_folder("restored");
        let path = Path::new("/work/app.feat");
        mark_restored(&folder, path, "abc123");

        assert!(is_kept(&folder, path, "abc123"));
        assert!(!is_kept(&folder, path, "def456"), "a new commit");
        assert!(!is_kept(&folder, Path::new("/work/app.other"), "abc123"));
    }
}
