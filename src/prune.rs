//! The worktrees of the merged pull requests (option `prune_merged`).
//!
//! A worktree can stay after the merge of its pull request: its agent
//! stopped before the merge, or `wt switch <branch>` made the worktree
//! again for a change of the review. The observer finds these worktrees,
//! and removes them with wt.
//!
//! A worktree is merged if GitHub merged a pull request of its branch, if
//! the default branch does not contain its local commit, and if one of
//! these conditions is true:
//!
//! - The last commit of the pull request is the local commit of the branch.
//! - The pull request contains the local commit. This case covers the
//!   button "Update branch" of GitHub: it adds a commit that the local
//!   branch does not have.
//!
//! A local commit in the default branch is not work of the branch: a new
//! worktree starts there. Thus a new worktree stays, also on the name of
//! an old branch. `wt step prune` removes a worktree whose commit is in the
//! default branch, for example after a merge commit.
//!
//! The observer does not fetch `origin`. Thus the local copy of the default
//! branch of `origin` can be old. An old copy contains fewer commits, so
//! the check can only remove a worktree of a merged pull request.

use std::path::{Path, PathBuf};

use crate::git::{self, LinkedWorktree};
use crate::github;
use crate::herdr::Error;

/// A worktree of a merged pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// The local commit of the branch that GitHub merged. If the branch
    /// gets a new commit, this new commit is not in the pull request: the
    /// observer then keeps the branch.
    pub commit: String,
}

impl MergedWorktree {
    /// Return true if the branch still has the merged commit.
    /// `current_commit` is the commit of the branch now, or None if the
    /// branch does not exist.
    pub fn is_unchanged(&self, current_commit: Option<&str>) -> bool {
        current_commit == Some(self.commit.as_str())
    }
}

/// Where the local commit of a branch is, compared with the default branch
/// of `origin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultBranch {
    /// The default branch contains the local commit.
    Contains,
    /// The default branch does not contain the local commit.
    DoesNotContain,
    /// git does not know the default branch of `origin`.
    Unknown,
}

/// Decide if a pull request merged the local commit of a branch.
///
/// - `local_commit`: the commit of the local branch.
/// - `merged_heads`: the last commit of each merged pull request of the
///   branch.
/// - `default_branch`: where the local commit is. If the default branch is
///   not known, only the first condition applies.
/// - `head_contains`: tells if a pull request head contains the local
///   commit (a call to GitHub).
pub fn is_merged(
    local_commit: &str,
    merged_heads: &[String],
    default_branch: DefaultBranch,
    mut head_contains: impl FnMut(&str) -> bool,
) -> bool {
    if default_branch == DefaultBranch::Contains {
        return false;
    }
    if merged_heads.iter().any(|head| head == local_commit) {
        return true;
    }
    if default_branch == DefaultBranch::Unknown {
        return false;
    }
    merged_heads.iter().any(|head| head_contains(head))
}

/// Return the merged worktrees of the repository of `main_checkout`.
///
/// `is_busy` tells if an agent works in a worktree: the function skips
/// these worktrees, and does not ask GitHub about them. If the remote
/// `origin` is not on GitHub, or if git or gh fails, the function returns
/// an error and no worktree.
pub fn merged_worktrees(
    main_checkout: &Path,
    is_busy: impl Fn(&Path) -> bool,
) -> Result<Vec<MergedWorktree>, Error> {
    let worktrees: Vec<LinkedWorktree> = git::linked_worktrees(main_checkout)?
        .into_iter()
        .filter(|worktree| !is_busy(&worktree.path))
        .collect();
    if worktrees.is_empty() {
        return Ok(Vec::new());
    }
    let origin_url = git::origin_url(main_checkout)
        .ok_or_else(|| Error::Command("no remote origin".to_owned()))?;
    let repository = github::repository_of(&origin_url)
        .ok_or_else(|| Error::Command(format!("origin is not on github.com: {origin_url}")))?;
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
        let place = match &default_branch {
            None => DefaultBranch::Unknown,
            Some(branch) if git::is_ancestor(main_checkout, &local_commit, branch) => {
                DefaultBranch::Contains
            }
            Some(_) => DefaultBranch::DoesNotContain,
        };
        let is_merged = is_merged(&local_commit, &merged_heads, place, |head| {
            github::contains(&repository, head, &local_commit)
        });
        if is_merged {
            merged.push(MergedWorktree {
                path: worktree.path,
                branch: worktree.branch,
                commit: local_commit,
            });
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
        let merged = is_merged(
            "aaa",
            &heads(&["bbb", "aaa"]),
            DefaultBranch::DoesNotContain,
            never,
        );

        assert!(merged);
    }

    #[test]
    fn the_same_commit_is_merged_if_the_default_branch_is_not_known() {
        assert!(is_merged(
            "aaa",
            &heads(&["aaa"]),
            DefaultBranch::Unknown,
            never
        ));
    }

    #[test]
    fn a_new_worktree_on_the_name_of_a_merged_branch_stays() {
        // The pull request was merged with a fast-forward: the default
        // branch contains its head, and the new worktree starts there.
        let merged = is_merged("aaa", &heads(&["aaa"]), DefaultBranch::Contains, never);

        assert!(!merged);
    }

    #[test]
    fn a_pull_request_that_contains_the_local_commit_is_merged() {
        let merged = is_merged(
            "aaa",
            &heads(&["bbb"]),
            DefaultBranch::DoesNotContain,
            |head| head == "bbb",
        );

        assert!(merged);
    }

    #[test]
    fn a_pull_request_that_does_not_contain_the_local_commit_is_not_merged() {
        let merged = is_merged(
            "aaa",
            &heads(&["bbb"]),
            DefaultBranch::DoesNotContain,
            |_| false,
        );

        assert!(!merged);
    }

    #[test]
    fn github_is_not_asked_if_the_default_branch_is_not_known() {
        assert!(!is_merged(
            "aaa",
            &heads(&["bbb"]),
            DefaultBranch::Unknown,
            never
        ));
    }

    fn merged_worktree() -> MergedWorktree {
        MergedWorktree {
            path: PathBuf::from("/work/repo.feature"),
            branch: "feature".to_owned(),
            commit: "aaa".to_owned(),
        }
    }

    #[test]
    fn a_branch_with_the_merged_commit_is_unchanged() {
        assert!(merged_worktree().is_unchanged(Some("aaa")));
    }

    #[test]
    fn a_branch_with_a_new_commit_is_changed() {
        assert!(!merged_worktree().is_unchanged(Some("bbb")));
    }

    #[test]
    fn a_deleted_branch_is_changed() {
        assert!(!merged_worktree().is_unchanged(None));
    }

    #[test]
    fn a_branch_without_merged_pull_request_is_not_merged() {
        assert!(!is_merged("aaa", &[], DefaultBranch::DoesNotContain, never));
    }
}
