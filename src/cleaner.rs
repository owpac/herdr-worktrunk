//! The popup cleaner: the list of the old worktrees without pull request,
//! in fzf. `Enter` deletes one worktree, `ctrl-a` deletes all of them.
//! Each deletion goes through the popup confirm, then the list comes back.
//!
//! The action clean opens it for all the repositories of the herdr
//! workspaces. The daily pass of the observer (`stale_days`) opens it with
//! its own list.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::git::{self, canonical};
use crate::herdr::{self, Error};
use crate::picker::{self, shell_quote, FZF_CANCEL_EXIT_CODE, FZF_NO_MATCH_EXIT_CODE};
use crate::prune;
use crate::stale::{self, OldWorktree};

const DELETE_ALL_REQUEST: &str = "delete-all";
const HEADER: &str = "Enter: delete · ctrl-a: delete all · Tab: preview · Esc: quit";

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
}

/// Find the old worktrees of `scope` (one repository), or of all the
/// repositories of the herdr workspaces.
pub fn find_old_worktrees(scope: Option<&Path>, stale_days: u32) -> Result<Search, Error> {
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
    let results: Vec<(PathBuf, Result<Vec<OldWorktree>, Error>)> = thread::scope(|scope| {
        let handles: Vec<_> = repositories
            .into_iter()
            .map(|repository| {
                scope.spawn(move || {
                    let found = old_worktrees_of(&repository, is_busy, stale_days);
                    (repository, found)
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    let mut search = Search::default();
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
    search.old.sort_by_key(|worktree| std::cmp::Reverse(worktree.days));
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

/// The line of a worktree in fzf: 3 hidden fields (path, branch, main
/// checkout), then the shown text.
fn row(worktree: &OldWorktree) -> String {
    let repository = worktree
        .main_checkout
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "{}\t{}\t{}\t{repository:<24} {:<40} {} days",
        worktree.path.display(),
        worktree.branch,
        worktree.main_checkout.display(),
        worktree.branch,
        worktree.days
    )
}

/// Show the list in fzf, and return the choice of the user. `list_path` is
/// the file of the list: the preview keeps its tab next to it. `warning`
/// goes under the header, for example the repositories that the search
/// could not check.
pub fn choose(
    old: &[OldWorktree],
    list_path: &Path,
    warning: Option<&str>,
) -> Result<Option<Choice>, Error> {
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let binary = shell_quote(&executable.to_string_lossy());
    let cache = shell_quote(&list_path.to_string_lossy());
    let header = match warning {
        Some(warning) => format!("{HEADER}\n{warning}"),
        None => HEADER.to_owned(),
    };
    let mut fzf = Command::new("fzf")
        .args([
            "--ansi",
            "--layout",
            "reverse",
            "--no-sort",
            "--delimiter",
            "\t",
            "--with-nth",
            "4..",
            "--prompt",
            "clean> ",
            "--header",
            &header,
            "--preview-window",
            "right,50%",
            "--preview-label",
            &picker::preview_label(0),
        ])
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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Command(format!("cannot run fzf: {error}")))?;
    if let Some(mut input) = fzf.stdin.take() {
        let rows: String = old.iter().map(|worktree| row(worktree) + "\n").collect();
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
    fn a_row_hides_3_fields_and_shows_the_repository_the_branch_and_the_days() {
        let line = row(&old()[0]);
        let fields: Vec<&str> = line.split('\t').collect();

        assert_eq!(
            &fields[..3],
            ["/work/repo.old", "feature/old", "/work/repo"]
        );
        assert!(fields[3].starts_with("repo "));
        assert!(fields[3].ends_with("12 days"));
    }

    #[test]
    fn the_output_of_fzf_gives_the_choice() {
        let old = old();

        assert_eq!(parse_choice("delete-all\n", &old), Some(Choice::DeleteAll));
        assert_eq!(
            parse_choice(&format!("{}\n", row(&old[0])), &old),
            Some(Choice::Delete(old[0].clone()))
        );
        assert_eq!(parse_choice("", &old), None);
    }
}
