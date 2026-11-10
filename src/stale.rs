//! The old worktrees without pull request (option `stale_days`).
//!
//! Once a day, the thread of the merged pull requests looks for the
//! linked worktrees with no pull request, no agent that works, no change
//! that is not committed, and no activity for `stale_days` days. It opens
//! the popup cleaner of the plugin with these worktrees: the user deletes
//! some or all of them, or quits. A worktree where an idle agent waits
//! goes to the group "Waiting" of the cleaner: the user resumes or closes
//! the agent. The thread does not ask again about a worktree for
//! `stale_days` days. The action clean opens the same popup at once.
//!
//! 2 files in the state folder keep the dates, so a restart of the
//! observer does not ask again:
//!
//! - `stale-last`: the time of the last pass.
//! - `stale-asked`: one line for each question, `<time>\t<path>`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The number of seconds in a day.
pub const DAY: u64 = 86_400;

/// The whole days between the time `time` and `now`, in seconds since the
/// epoch. A time in the future gives 0.
pub fn days_since(time: u64, now: u64) -> u64 {
    now.saturating_sub(time) / DAY
}

/// The time now, in seconds since 1970.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Return true if `last_activity` is `stale_days` days old or older.
pub fn is_stale(last_activity: u64, now: u64, stale_days: u32) -> bool {
    stale_days > 0 && now.saturating_sub(last_activity) >= u64::from(stale_days) * DAY
}

/// Return true if the last pass is one day old or older, or if no pass
/// ran yet.
pub fn is_pass_due(last_pass: Option<u64>, now: u64) -> bool {
    last_pass.is_none_or(|time| now.saturating_sub(time) >= DAY)
}

/// Return true if the thread did not ask about `path` in the last
/// `stale_days` days.
pub fn may_ask(asked: &HashMap<PathBuf, u64>, path: &Path, now: u64, stale_days: u32) -> bool {
    asked
        .get(path)
        .is_none_or(|&time| now.saturating_sub(time) >= u64::from(stale_days) * DAY)
}

fn last_pass_path(state_dir: &Path) -> PathBuf {
    state_dir.join("stale-last")
}

fn asked_path(state_dir: &Path) -> PathBuf {
    state_dir.join("stale-asked")
}

/// Read the time of the last pass, or None if no pass ran yet.
pub fn read_last_pass(state_dir: &Path) -> Option<u64> {
    fs::read_to_string(last_pass_path(state_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Write the time of the last pass. A failed write only makes the next
/// pass come sooner.
pub fn write_last_pass(state_dir: &Path, time: u64) {
    let _ = fs::write(last_pass_path(state_dir), time.to_string());
}

/// Read the time of each question, by path of the worktree.
pub fn read_asked(state_dir: &Path) -> HashMap<PathBuf, u64> {
    fs::read_to_string(asked_path(state_dir))
        .map(|text| parse_asked(&text))
        .unwrap_or_default()
}

/// Write the time of each question. The file keeps only the worktrees
/// that still exist.
pub fn write_asked(state_dir: &Path, asked: &HashMap<PathBuf, u64>) {
    let kept: HashMap<PathBuf, u64> = asked
        .iter()
        .filter(|(path, _)| path.exists())
        .map(|(path, time)| (path.clone(), *time))
        .collect();
    let _ = fs::write(asked_path(state_dir), format_asked(&kept));
}

fn parse_asked(text: &str) -> HashMap<PathBuf, u64> {
    text.lines()
        .filter_map(|line| {
            let (time, path) = line.split_once('\t')?;
            Some((PathBuf::from(path), time.parse().ok()?))
        })
        .collect()
}

fn format_asked(asked: &HashMap<PathBuf, u64>) -> String {
    let mut lines: Vec<String> = asked
        .iter()
        .map(|(path, time)| format!("{time}\t{}\n", path.display()))
        .collect();
    lines.sort();
    lines.concat()
}

/// An old worktree without pull request: a candidate of the cleaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OldWorktree {
    pub main_checkout: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// The days without activity.
    pub days: u64,
}

/// Select the old worktrees among the worktrees without pull request of
/// the repository of `main_checkout`: no agent (`is_busy`), no activity
/// for `stale_days` days, and no change that is not committed.
///
/// `asked` gives the last questions of the daily pass: a worktree gets a
/// new question only after `stale_days` days. A manual cleaning gives
/// None: it shows all the old worktrees.
pub fn select_old(
    main_checkout: &Path,
    worktrees: Vec<crate::git::LinkedWorktree>,
    is_busy: impl Fn(&Path) -> bool,
    asked: Option<&HashMap<PathBuf, u64>>,
    stale_days: u32,
) -> Vec<OldWorktree> {
    let now = now();
    worktrees
        .into_iter()
        .filter_map(|worktree| {
            if is_busy(&worktree.path) {
                return None;
            }
            let path = crate::git::canonical(&worktree.path);
            if asked.is_some_and(|asked| !may_ask(asked, &path, now, stale_days)) {
                return None;
            }
            let activity = crate::git::last_activity(&path)?;
            if !is_stale(activity, now, stale_days) || !crate::git::is_clean(&path) {
                return None;
            }
            Some(OldWorktree {
                main_checkout: main_checkout.to_path_buf(),
                path,
                branch: worktree.branch,
                days: days_since(activity, now),
            })
        })
        .collect()
}

/// Write a file in one rename: a reader never sees a part of it, also if
/// the writer stops. The temporary file has the name of the file and the
/// process: 2 files or 2 processes never share it.
pub fn write_atomically(path: &Path, text: &str) -> Result<(), std::io::Error> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new_path = path.with_file_name(format!(".{name}.{}.new", std::process::id()));
    fs::write(&new_path, text)?;
    fs::rename(&new_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn a_worktree_without_activity_for_the_given_days_is_stale() {
        assert!(is_stale(NOW - 7 * DAY, NOW, 7));
        assert!(!is_stale(NOW - 6 * DAY, NOW, 7));
    }

    #[test]
    fn stale_days_0_turns_the_question_off() {
        assert!(!is_stale(0, NOW, 0));
    }

    #[test]
    fn a_pass_runs_once_a_day() {
        assert!(is_pass_due(None, NOW));
        assert!(is_pass_due(Some(NOW - DAY), NOW));
        assert!(!is_pass_due(Some(NOW - DAY + 60), NOW));
    }

    #[test]
    fn the_thread_does_not_ask_again_before_the_given_days() {
        let path = PathBuf::from("/work/repo.old");
        let asked = HashMap::from([(path.clone(), NOW - 3 * DAY)]);

        assert!(!may_ask(&asked, &path, NOW, 7));
        assert!(may_ask(&asked, &path, NOW + 4 * DAY, 7));
        assert!(may_ask(&asked, Path::new("/work/repo.new"), NOW, 7));
    }

    #[test]
    fn the_questions_are_read_back() {
        let asked = HashMap::from([
            (PathBuf::from("/work/repo.a"), 10),
            (PathBuf::from("/work/repo b"), 20),
        ]);

        assert_eq!(parse_asked(&format_asked(&asked)), asked);
        assert!(parse_asked("not a line\n").is_empty());
    }
}
