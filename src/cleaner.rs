//! The popup cleaner: the list of the old worktrees without pull request,
//! in fzf. `Enter` deletes one worktree, `ctrl-a` deletes all of them.
//! Each deletion goes through the popup confirm, then the list comes back.
//!
//! The action clean opens it for all the repositories of the herdr
//! workspaces. The daily pass of the observer (`stale_days`) opens it with
//! its own list.
//!
//! The style comes from the module `style`, as for the picker.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::git::{self, canonical, LinkedWorktree};
use crate::github;
use crate::herdr::{self, Error};
use crate::picker::{shell_quote, FZF_CANCEL_EXIT_CODE, FZF_NO_MATCH_EXIT_CODE};
use crate::prune;
use crate::stale::{self, OldWorktree};
use crate::style::{self, part, DIM, MUTED, REPOSITORY, RULE};

const DELETE_ALL_REQUEST: &str = "delete-all";
const GHOST: &str = "old worktree — type to filter";
const HINTS: [(&str, &str); 5] = [
    ("↑↓", "move"),
    ("⏎", "delete"),
    ("^a", "delete all"),
    ("⇥", "preview"),
    ("esc", "close"),
];

/// What the user chose in the list.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Delete(OldWorktree),
    DeleteAll,
}

/// The main checkouts of the repositories of the herdr workspaces, and
/// the checkouts that an agent uses (its folder, or its workspace).
fn herdr_repositories() -> Result<(Vec<PathBuf>, HashSet<PathBuf>), Error> {
    let snapshot = herdr::snapshot()?;
    let mut repositories = Vec::new();
    let mut workspace_roots = std::collections::HashMap::new();
    for workspace in &snapshot.workspaces {
        let Ok(Some(checkout)) = herdr::workspace_checkout(&workspace.workspace_id) else {
            continue;
        };
        workspace_roots.insert(
            workspace.workspace_id.clone(),
            canonical(&checkout.checkout_path),
        );
        if let Some(found) = git::checkout_of(&checkout.checkout_path) {
            let main = canonical(&found.main_checkout());
            if !repositories.contains(&main) {
                repositories.push(main);
            }
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

/// The result of a search of the old worktrees.
#[derive(Debug, Default)]
pub struct Search {
    pub old: Vec<OldWorktree>,
    /// The repositories that the search could not check (gh not
    /// authenticated, no remote on GitHub...), with the reason.
    pub failures: Vec<String>,
    /// The list uses the file of the observer for at least one
    /// repository: a pull request can be younger than this file.
    pub uses_observer: bool,
}

/// Find the old worktrees of `scope` (one repository), or of all the
/// repositories of the herdr workspaces.
///
/// `known` gives the worktrees without pull request that the observer
/// found at its last pass (`no-pull-request.tsv`). For these
/// repositories, the search does the local checks only, without GitHub.
pub fn find_old_worktrees(
    scope: Option<&Path>,
    stale_days: u32,
    known: Option<&HashMap<PathBuf, Vec<LinkedWorktree>>>,
) -> Result<Search, Error> {
    let (mut repositories, busy) = herdr_repositories()?;
    if let Some(scope) = scope {
        let main = git::checkout_of(scope)
            .map(|checkout| canonical(&checkout.main_checkout()))
            .ok_or_else(|| {
                Error::Command(format!("{} is not in a git repository", scope.display()))
            })?;
        repositories = vec![main];
    }
    let is_busy = |path: &Path| busy.contains(&canonical(path));
    // One thread for each repository: gh takes about half a second for
    // each old worktree.
    let uses_observer = known.is_some_and(|known| {
        repositories
            .iter()
            .any(|repository| known.contains_key(repository))
    });
    let results: Vec<(PathBuf, Result<Vec<OldWorktree>, Error>)> = thread::scope(|scope| {
        let handles: Vec<_> = repositories
            .into_iter()
            .map(|repository| {
                scope.spawn(move || {
                    let found = match known.and_then(|known| known.get(&repository)) {
                        Some(worktrees) => Ok(stale::select_old(
                            &repository,
                            worktrees.clone(),
                            is_busy,
                            None,
                            stale_days,
                        )),
                        None => old_worktrees_of(&repository, is_busy, stale_days),
                    };
                    (repository, found)
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    let mut search = Search {
        uses_observer,
        ..Search::default()
    };
    for (repository, found) in results {
        match found {
            Ok(old) => search.old.extend(old),
            Err(error) => {
                let name = repository
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                search
                    .failures
                    .push(format!("{name}: {}", error.to_string().replace('\n', " ")));
            }
        }
    }
    search
        .old
        .sort_by_key(|worktree| std::cmp::Reverse(worktree.days));
    Ok(search)
}

/// The old worktrees of one repository: the local checks first, then gh
/// for the old worktrees only.
fn old_worktrees_of(
    repository: &Path,
    is_busy: impl Fn(&Path) -> bool,
    stale_days: u32,
) -> Result<Vec<OldWorktree>, Error> {
    let worktrees = git::linked_worktrees(repository)?;
    let old = stale::select_old(repository, worktrees, is_busy, None, stale_days);
    prune::without_pull_request(repository, old)
}

/// Return true if the branch of `worktree` has a pull request on GitHub,
/// or None if gh or the remote fails.
fn has_pull_request(worktree: &OldWorktree) -> Option<bool> {
    let origin_url = git::origin_url(&worktree.main_checkout)?;
    let repository = github::repository_of(&origin_url)?;
    github::pull_requests(&repository, &worktree.branch)
        .ok()
        .map(|pull_requests| !pull_requests.is_empty())
}

/// fzf event `cleaner-verify`: check the pull requests of the worktrees of
/// the list, one thread for each, then write the list again without the
/// worktrees that have one, and print its rows. fzf shows the list at
/// once, then these rows: a pull request younger than the list leaves the
/// list a second later. An error of gh keeps the worktree.
pub fn print_verified_rows(list_path: &Path) {
    let old = stale::read_list(list_path);
    let kept: Vec<OldWorktree> = thread::scope(|scope| {
        let handles: Vec<_> = old
            .into_iter()
            .map(|worktree| {
                scope.spawn(move || (has_pull_request(&worktree) != Some(true)).then_some(worktree))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect()
    });
    let _ = stale::write_list(list_path, &kept);
    print_rows(&kept);
}

/// fzf event `cleaner-rows`: print the rows of the list again, with the
/// new width of the popup.
pub fn print_list_rows(list_path: &Path) {
    print_rows(&stale::read_list(list_path));
}

fn print_rows(old: &[OldWorktree]) {
    let width = style::row_width(style::fzf_columns());
    for worktree in old {
        println!("{}", row(worktree, width));
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The line of a worktree in fzf: 3 hidden fields (path, branch, main
/// checkout), then the shown text in `width` columns: the branch and its
/// repository on the left, the folder and the days without activity on
/// the right.
fn row(worktree: &OldWorktree, width: usize) -> String {
    let left = [
        part(DIM, "○ "),
        part("", worktree.branch.clone()),
        part(RULE, " — "),
        part(REPOSITORY, file_name(&worktree.main_checkout)),
    ];
    let right = [
        part(MUTED, file_name(&worktree.path)),
        part(MUTED, format!("{}d", worktree.days)),
    ];
    format!(
        "{}\t{}\t{}\t{}",
        worktree.path.display(),
        worktree.branch,
        worktree.main_checkout.display(),
        style::row(&left, &right, width)
    )
}

/// Show the list in fzf, and return the choice of the user. `list_path` is
/// the file of the list: the preview keeps its tab next to it. `scope`
/// shows on the right of the prompt: a repository, or all the
/// repositories. `warning` goes in the header, for example the
/// repositories that the search could not check. With `verify`, fzf
/// checks the pull requests of the list after it shows it
/// (`cleaner-verify`), and reloads the list.
pub fn choose(
    old: &[OldWorktree],
    list_path: &Path,
    scope: &str,
    warning: Option<&str>,
    verify: bool,
) -> Result<Option<Choice>, Error> {
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let binary = shell_quote(&executable.to_string_lossy());
    let cache = shell_quote(&list_path.to_string_lossy());
    let columns = style::terminal_columns();
    let mut fzf = Command::new("fzf");
    fzf.env(style::COLUMNS_VARIABLE, columns.to_string());
    if let Some(warning) = warning {
        fzf.arg("--header").arg(warning);
    }
    if verify {
        // `load` comes once fzf has read the list: the check runs once,
        // and reload-sync keeps the list on the screen until it ends.
        fzf.arg("--bind").arg(format!(
            "load:unbind(load)+reload-sync({binary} cleaner-verify {cache})"
        ));
    }
    let mut fzf = fzf
        .args(style::fzf_arguments(GHOST, scope, &HINTS))
        .arg("--preview")
        .arg(format!(
            "{binary} picker-preview {{3}} {cache} worktree {{2}} {{1}}"
        ))
        .arg("--bind")
        .arg(format!("tab:transform({binary} picker-tab {cache} next)"))
        .arg("--bind")
        .arg(format!(
            "btab:transform({binary} picker-tab {cache} previous)"
        ))
        .arg("--bind")
        .arg(format!("ctrl-a:print({DELETE_ALL_REQUEST})+accept"))
        .arg("--bind")
        .arg(format!("resize:reload({binary} cleaner-rows {cache})"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Command(format!("cannot run fzf: {error}")))?;
    if let Some(mut input) = fzf.stdin.take() {
        let width = style::row_width(columns);
        let rows: String = old
            .iter()
            .map(|worktree| row(worktree, width) + "\n")
            .collect();
        // fzf closes its input when the user quits: a failed write is not
        // an error.
        let _ = input.write_all(rows.as_bytes());
    }
    let output = fzf
        .wait_with_output()
        .map_err(|error| Error::Command(format!("fzf failed: {error}")))?;
    let _ = fs::remove_file(list_path.with_extension("tab"));
    match output.status.code() {
        Some(0) => {}
        Some(FZF_CANCEL_EXIT_CODE) | Some(FZF_NO_MATCH_EXIT_CODE) => return Ok(None),
        _ => return Err(Error::Command("fzf failed.".to_owned())),
    }
    Ok(parse_choice(&String::from_utf8_lossy(&output.stdout), old))
}

/// Read the output of fzf: `delete-all`, or the line of one worktree.
fn parse_choice(text: &str, old: &[OldWorktree]) -> Option<Choice> {
    let first = text.lines().next()?;
    if first == DELETE_ALL_REQUEST {
        return Some(Choice::DeleteAll);
    }
    let path = first.split('\t').next()?;
    old.iter()
        .find(|worktree| worktree.path == Path::new(path))
        .cloned()
        .map(Choice::Delete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old() -> Vec<OldWorktree> {
        vec![OldWorktree {
            main_checkout: PathBuf::from("/work/repo"),
            path: PathBuf::from("/work/repo.old"),
            branch: "feature/old".to_owned(),
            days: 12,
        }]
    }

    #[test]
    fn a_row_hides_3_fields_and_shows_the_branch_the_repository_and_the_days() {
        let line = row(&old()[0], 60);
        let fields: Vec<&str> = line.split('\t').collect();

        assert_eq!(
            &fields[..3],
            ["/work/repo.old", "feature/old", "/work/repo"]
        );
        assert!(fields[3].contains("feature/old"));
        assert!(fields[3].contains("repo"));
        assert!(fields[3].contains("repo.old"));
        assert!(fields[3].contains("12d"));
    }

    #[test]
    fn the_output_of_fzf_gives_the_choice() {
        let old = old();

        assert_eq!(parse_choice("delete-all\n", &old), Some(Choice::DeleteAll));
        assert_eq!(
            parse_choice(&format!("{}\n", row(&old[0], 60)), &old),
            Some(Choice::Delete(old[0].clone()))
        );
        assert_eq!(parse_choice("", &old), None);
    }
}
