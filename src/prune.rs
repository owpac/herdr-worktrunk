//! The worktrees of the merged pull requests (option `prune_merged`).
//!
//! A worktree can stay after the merge of its pull request: its agent
//! stopped before the merge, or `wt switch <branch>` made the worktree
//! again for a change of the review. The observer finds these worktrees,
//! and removes them with wt.
//!
//! A worktree is merged if GitHub merged a pull request of its branch, and
//! if one of these conditions is true:
//!
//! - The last commit of the pull request is the local commit of the branch.
//! - The pull request contains the local commit, and the default branch
//!   does not. This case covers the button "Update branch" of GitHub: it
//!   adds a commit that the local branch does not have.
//!
//! A new worktree has no merged pull request, or a different commit, so it
//! stays.

use std::path::Path;

use crate::git::{self, LinkedWorktree};
use crate::github;
use crate::herdr::Error;

/// Decide if a pull request merged the local commit of a branch.
///
/// - `local_commit`: the commit of the local branch.
/// - `merged_heads`: the last commit of each merged pull request of the
///   branch.
/// - `is_in_default_branch`: true if the default branch contains the local
///   commit, or if the default branch is not known. A local commit in the
///   default branch is not work of the branch: a new worktree starts there.
/// - `head_contains`: tells if a pull request head contains the local
///   commit (a call to GitHub).
pub fn is_merged(
    local_commit: &str,
    merged_heads: &[String],
    is_in_default_branch: bool,
    mut head_contains: impl FnMut(&str) -> bool,
) -> bool {
    if merged_heads.iter().any(|head| head == local_commit) {
        return true;
    }
    if is_in_default_branch {
        return false;
    }
    merged_heads.iter().any(|head| head_contains(head))
}

/// Return the merged worktrees of the repository of `main_checkout`.
///
/// `is_busy` tells if an agent works in a worktree: the function skips
/// these worktrees, and does not ask GitHub about them. If the remote
/// `origin` is not on GitHub, the list is empty. If git or gh fails, the
/// function returns the error and no worktree.
pub fn merged_worktrees(
    main_checkout: &Path,
    is_busy: impl Fn(&Path) -> bool,
) -> Result<Vec<LinkedWorktree>, Error> {
    let worktrees: Vec<LinkedWorktree> = git::linked_worktrees(main_checkout)?
        .into_iter()
        .filter(|worktree| !is_busy(&worktree.path))
        .collect();
    if worktrees.is_empty() {
        return Ok(Vec::new());
    }
    let Some(repository) = git::origin_url(main_checkout)
        .as_deref()
        .and_then(github::repository_of)
    else {
        return Ok(Vec::new());
    };
    git::fetch(main_checkout)?;
    let default_branch = git::default_branch(main_checkout);
    let mut merged = Vec::new();
    for worktree in worktrees {
        let merged_heads = github::merged_heads(&repository, &worktree.branch)?;
        if merged_heads.is_empty() {
            continue;
        }
        let Some(local_commit) = git::branch_commit(main_checkout, &worktree.branch) else {
            continue;
        };
        let is_in_default_branch = default_branch
            .as_deref()
            .is_none_or(|branch| git::is_ancestor(main_checkout, &local_commit, branch));
        let is_merged = is_merged(&local_commit, &merged_heads, is_in_default_branch, |head| {
            github::contains(&repository, head, &local_commit)
        });
        if is_merged {
            merged.push(worktree);
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heads(commits: &[&str]) -> Vec<String> {
        commits.iter().map(|commit| (*commit).to_owned()).collect()
    }

    fn never(_head: &str) -> bool {
        panic!("GitHub must not be asked");
    }

    #[test]
    fn the_same_commit_as_a_merged_pull_request_is_merged() {
        assert!(is_merged("aaa", &heads(&["bbb", "aaa"]), true, never));
    }

    #[test]
    fn a_local_commit_in_the_default_branch_is_not_merged() {
        assert!(!is_merged("aaa", &heads(&["bbb"]), true, never));
    }

    #[test]
    fn a_pull_request_that_contains_the_local_commit_is_merged() {
        let merged = is_merged("aaa", &heads(&["bbb"]), false, |head| head == "bbb");

        assert!(merged);
    }

    #[test]
    fn a_pull_request_that_does_not_contain_the_local_commit_is_not_merged() {
        assert!(!is_merged("aaa", &heads(&["bbb"]), false, |_| false));
    }

    #[test]
    fn a_branch_without_merged_pull_request_is_not_merged() {
        assert!(!is_merged("aaa", &[], false, never));
    }
}
