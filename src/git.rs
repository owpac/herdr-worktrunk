//! The git facts that the plugin needs about a folder.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::herdr::Error;

/// A git checkout: the main checkout or a linked worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The root folder of the checkout.
    pub root: PathBuf,
    /// The git folder that all the checkouts of the repository share.
    /// Two checkouts with the same common folder are in the same
    /// repository.
    pub common_dir: PathBuf,
}

impl Checkout {
    /// The main checkout of the repository: the parent of the common
    /// git folder.
    pub fn main_checkout(&self) -> PathBuf {
        match self.common_dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => self.root.clone(),
        }
    }
}

/// Return the checkout that holds `folder`, or None if `folder` is not in
/// a git checkout (or does not exist).
pub fn checkout_of(folder: &Path) -> Option<Checkout> {
    let output = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let root = PathBuf::from(lines.next()?);
    let common_dir = PathBuf::from(lines.next()?);
    Some(Checkout { root, common_dir })
}

/// Return the branch of the checkout `root`, or None for a detached HEAD.
pub fn branch_of(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A linked worktree of a repository, on a branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedWorktree {
    pub path: PathBuf,
    pub branch: String,
}

/// Run git in `folder` without a terminal: git cannot ask for a password.
/// Return its output, or its error message.
fn run_in(folder: &Path, arguments: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::Command(format!("cannot run git: {error}")))?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Command(format!(
            "git {}: {}",
            arguments.first().copied().unwrap_or_default(),
            details.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Return the linked worktrees of the repository that the observer can
/// remove. See `parse_worktrees`.
pub fn linked_worktrees(main_checkout: &Path) -> Result<Vec<LinkedWorktree>, Error> {
    let porcelain = run_in(main_checkout, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktrees(&porcelain))
}

/// Read the output of `git worktree list --porcelain`.
///
/// The output has one block for each worktree, the main worktree first.
/// A block has the lines `worktree <path>`, `branch refs/heads/<name>`,
/// and optionally `detached`, `locked` or `prunable`. An empty line ends
/// a block. The result does not contain the main worktree, a detached
/// worktree, a locked worktree or a prunable worktree.
fn parse_worktrees(porcelain: &str) -> Vec<LinkedWorktree> {
    let mut worktrees = Vec::new();
    for block in porcelain.split("\n\n").skip(1) {
        let mut path = None;
        let mut branch = None;
        let mut keep = false;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
                branch = Some(value.to_owned());
            } else if line == "detached"
                || line.starts_with("locked")
                || line.starts_with("prunable")
            {
                keep = true;
            }
        }
        if let (Some(path), Some(branch), false) = (path, branch, keep) {
            worktrees.push(LinkedWorktree { path, branch });
        }
    }
    worktrees
}

/// Return the URL of the remote `origin`, or None if it does not exist.
pub fn origin_url(main_checkout: &Path) -> Option<String> {
    run_in(main_checkout, &["remote", "get-url", "origin"]).ok()
}

/// Fetch `origin`, and delete the remote branches that `origin` deleted.
pub fn fetch(main_checkout: &Path) -> Result<(), Error> {
    run_in(main_checkout, &["fetch", "--prune", "--quiet", "origin"]).map(drop)
}

/// Return the default branch of `origin` (for example `origin/main`), or
/// None if git does not know it.
pub fn default_branch(main_checkout: &Path) -> Option<String> {
    run_in(
        main_checkout,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()
}

/// Return the commit of the local branch `branch`.
pub fn branch_commit(main_checkout: &Path, branch: &str) -> Option<String> {
    let reference = format!("refs/heads/{branch}");
    run_in(
        main_checkout,
        &["rev-parse", "--verify", "--quiet", &reference],
    )
    .ok()
}

/// Return true if `commit` is in the history of `branch`.
pub fn is_ancestor(main_checkout: &Path, commit: &str, branch: &str) -> bool {
    run_in(
        main_checkout,
        &["merge-base", "--is-ancestor", commit, branch],
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_main_checkout_is_the_parent_of_the_common_folder() {
        let checkout = Checkout {
            root: PathBuf::from("/work/repo.feature"),
            common_dir: PathBuf::from("/work/repo/.git"),
        };

        assert_eq!(checkout.main_checkout(), PathBuf::from("/work/repo"));
    }

    #[test]
    fn a_folder_outside_git_has_no_checkout() {
        assert_eq!(checkout_of(Path::new("/")), None);
    }

    #[test]
    fn the_worktree_list_keeps_only_the_linked_worktrees_on_a_branch() {
        let porcelain = "\
worktree /work/repo
HEAD 1111
branch refs/heads/main

worktree /work/repo.feature
HEAD 2222
branch refs/heads/feature/auth

worktree /work/repo.detached
HEAD 3333
detached

worktree /work/repo.locked
HEAD 4444
branch refs/heads/locked
locked reason

worktree /work/repo.gone
HEAD 5555
branch refs/heads/gone
prunable gitdir file points to non-existent location
";

        assert_eq!(
            parse_worktrees(porcelain),
            [LinkedWorktree {
                path: PathBuf::from("/work/repo.feature"),
                branch: "feature/auth".to_owned(),
            }]
        );
    }

    #[test]
    fn a_repository_without_linked_worktree_has_an_empty_list() {
        let porcelain = "worktree /work/repo\nHEAD 1111\nbranch refs/heads/main\n";

        assert!(parse_worktrees(porcelain).is_empty());
    }
}
