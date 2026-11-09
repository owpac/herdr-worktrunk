//! The old worktrees without pull request (option `stale_days`).
//!
//! Once a day, the thread of the merged pull requests looks for the
//! linked worktrees with no pull request, no agent, no change that is not
//! committed, and no activity for `stale_days` days. It opens the popup
//! cleaner of the plugin with these worktrees: the user deletes some or
//! all of them, or quits. The thread does not ask again about a worktree
//! for `stale_days` days. The action clean opens the same popup at once.
//!
//! 2 files in the state folder keep the dates, so a restart of the
//! observer does not ask again:
//!
//! - `stale-last`: the time of the last pass.
//! - `stale-asked`: one line for each question, `<time>\t<path>`.
//!
//! At each pass (every 5 minutes), the thread also writes
//! `no-pull-request.tsv`: the worktrees without pull request of each
//! repository that it checked. The popup cleaner reads this file, so it
//! calls GitHub only for a repository that the thread did not check.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::git::LinkedWorktree;

/// The number of seconds in a day.
pub const DAY: u64 = 86_400;

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
                days: now.saturating_sub(activity) / DAY,
            })
        })
        .collect()
}

/// Write a list of old worktrees: one line for each worktree,
/// `<path>\t<branch>\t<main checkout>\t<days>`. The popup cleaner reads
/// this file.
pub fn write_list(path: &Path, old: &[OldWorktree]) -> Result<(), std::io::Error> {
    let text: String = old
        .iter()
        .map(|worktree| {
            format!(
                "{}\t{}\t{}\t{}\n",
                worktree.path.display(),
                worktree.branch,
                worktree.main_checkout.display(),
                worktree.days
            )
        })
        .collect();
    // Write a new file, then rename it: a reader never sees a part of the
    // list, also if the writer stops.
    let new_path = path.with_extension("new");
    fs::write(&new_path, text)?;
    fs::rename(&new_path, path)
}

/// Read a list of old worktrees, without the worktrees that do not exist
/// anymore (a deletion since the list was written).
pub fn read_list(path: &Path) -> Vec<OldWorktree> {
    fs::read_to_string(path)
        .map(|text| parse_list(&text))
        .unwrap_or_default()
        .into_iter()
        .filter(|worktree| worktree.path.exists())
        .collect()
}

fn parse_list(text: &str) -> Vec<OldWorktree> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some(OldWorktree {
                path: PathBuf::from(fields.next()?),
                branch: fields.next()?.to_owned(),
                main_checkout: PathBuf::from(fields.next()?),
                days: fields.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// The age after which the popup cleaner does not use
/// `no-pull-request.tsv`: 3 passes of the thread.
pub const NO_PULL_REQUEST_MAX_AGE: u64 = 15 * 60;

fn no_pull_request_path(state_dir: &Path) -> PathBuf {
    state_dir.join("no-pull-request.tsv")
}

/// Write the worktrees without pull request of the checked repositories.
/// A temporary file and a rename: the popup never reads a partial file.
///
/// Lines: `checked\t<time>`, then `repository\t<main checkout>` for each
/// checked repository, and `worktree\t<main checkout>\t<path>\t<branch>`
/// for each worktree without pull request.
pub fn write_no_pull_request(
    state_dir: &Path,
    checked: &[(PathBuf, Vec<LinkedWorktree>)],
    now: u64,
) -> Result<(), std::io::Error> {
    let mut text = format!("checked\t{now}\n");
    for (main_checkout, worktrees) in checked {
        text.push_str(&format!("repository\t{}\n", main_checkout.display()));
        for worktree in worktrees {
            text.push_str(&format!(
                "worktree\t{}\t{}\t{}\n",
                main_checkout.display(),
                worktree.path.display(),
                worktree.branch
            ));
        }
    }
    let path = no_pull_request_path(state_dir);
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::create_dir_all(state_dir)?;
    fs::write(&temporary, text)?;
    fs::rename(&temporary, &path)
}

/// Read the worktrees without pull request of each checked repository, or
/// None if the file is missing or older than `max_age` seconds. The
/// worktrees that do not exist anymore are left out.
pub fn read_no_pull_request(
    state_dir: &Path,
    now: u64,
    max_age: u64,
) -> Option<HashMap<PathBuf, Vec<LinkedWorktree>>> {
    let text = fs::read_to_string(no_pull_request_path(state_dir)).ok()?;
    let (checked_at, repositories) = parse_no_pull_request(&text)?;
    if now.saturating_sub(checked_at) > max_age {
        return None;
    }
    Some(
        repositories
            .into_iter()
            .map(|(main_checkout, worktrees)| {
                let existing = worktrees
                    .into_iter()
                    .filter(|worktree| worktree.path.exists())
                    .collect();
                (main_checkout, existing)
            })
            .collect(),
    )
}

type NoPullRequest = (u64, HashMap<PathBuf, Vec<LinkedWorktree>>);

fn parse_no_pull_request(text: &str) -> Option<NoPullRequest> {
    let mut checked_at = None;
    let mut repositories: HashMap<PathBuf, Vec<LinkedWorktree>> = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["checked", time] => checked_at = time.parse().ok(),
            ["repository", main_checkout] => {
                repositories
                    .entry(PathBuf::from(main_checkout))
                    .or_default();
            }
            ["worktree", main_checkout, path, branch] => repositories
                .entry(PathBuf::from(main_checkout))
                .or_default()
                .push(LinkedWorktree {
                    path: PathBuf::from(path),
                    branch: (*branch).to_owned(),
                }),
            _ => {}
        }
    }
    Some((checked_at?, repositories))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worktrees_without_pull_request_are_read_back_while_fresh() {
        let folder =
            std::env::temp_dir().join(format!("herdr-worktrunk-npr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let existing = folder.join("repo.old");
        fs::create_dir_all(&existing).unwrap();
        let checked = vec![
            (
                PathBuf::from("/work/repo"),
                vec![
                    LinkedWorktree {
                        path: existing.clone(),
                        branch: "old".to_owned(),
                    },
                    LinkedWorktree {
                        path: PathBuf::from("/gone"),
                        branch: "gone".to_owned(),
                    },
                ],
            ),
            (PathBuf::from("/work/empty"), Vec::new()),
        ];
        write_no_pull_request(&folder, &checked, 1000).unwrap();

        let read = read_no_pull_request(
            &folder,
            1000 + NO_PULL_REQUEST_MAX_AGE,
            NO_PULL_REQUEST_MAX_AGE,
        )
        .unwrap();
        assert_eq!(
            read[Path::new("/work/repo")],
            vec![LinkedWorktree {
                path: existing,
                branch: "old".to_owned()
            }]
        );
        assert!(read[Path::new("/work/empty")].is_empty());
        assert!(read_no_pull_request(
            &folder,
            1001 + NO_PULL_REQUEST_MAX_AGE,
            NO_PULL_REQUEST_MAX_AGE
        )
        .is_none());
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_list_of_old_worktrees_is_read_back() {
        let old = vec![OldWorktree {
            main_checkout: PathBuf::from("/work/repo"),
            path: PathBuf::from("/work/repo.old"),
            branch: "feature/old".to_owned(),
            days: 12,
        }];
        let text = "/work/repo.old\tfeature/old\t/work/repo\t12\nbad line\n";

        assert_eq!(parse_list(text), old);
    }

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
