//! The worktrees of the finished pull requests (option `prune_merged`).
//!
//! A worktree can stay after the merge or the close of its pull request:
//! its agent stays in it, or `wt switch <branch>` made the worktree again
//! for a change of the review. The observer finds these worktrees, and
//! removes them with wt.
//!
//! A pull request is finished if the forge merged it or closed it. A worktree
//! is finished if its branch has no open pull request, if the default
//! branch does not contain its local commit, and if one of these
//! conditions is true for a finished pull request of its branch:
//!
//! - The last commit of the pull request is the local commit of the branch.
//! - The pull request contains the local commit. This case covers the
//!   button "Update branch" of GitHub: it adds a commit that the local
//!   branch does not have.
//!
//! Thus the forge keeps each local commit of a finished worktree: in the
//! default branch after a merge, and with the pull request after a close.
//! GitHub keeps `refs/pull/<number>/head`. GitLab deletes
//! `refs/merge-requests/<iid>/head` 14 days after the close, but keeps the
//! commits: a fetch of the commit itself gets the branch back.
//!
//! A local commit in the default branch is not work of the branch: a new
//! worktree starts there. Thus a new worktree stays, also on the name of
//! an old branch. `wt step prune` removes a worktree whose commit is in the
//! default branch, for example after a merge commit.
//!
//! The observer does not fetch `origin`. Thus the local copy of the default
//! branch of `origin` can be old. An old copy contains fewer commits, so
//! the check can only remove a worktree of a finished pull request.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge::{self, Backend, PullRequest, PullRequestState};
use crate::git::{self, LinkedWorktree};
use crate::herdr::Error;
use crate::stale::OldWorktree;

/// A worktree of a finished pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishedWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// The local commit of the branch that the pull request contains. If
    /// the branch gets a new commit, this new commit is not in the pull
    /// request: the observer then keeps the branch.
    pub commit: String,
    /// The finished pull request that contains the local commit.
    pub pull_request: PullRequest,
    /// The command that gets back the branch after the removal of a closed
    /// pull request.
    pub recover_command: String,
    /// The pull request as the forge writes it: `pull request #3`.
    pub reference: String,
}

impl FinishedWorktree {
    /// Return true if the branch still has the checked commit.
    /// `current_commit` is the commit of the branch now, or None if the
    /// branch does not exist.
    pub fn is_unchanged(&self, current_commit: Option<&str>) -> bool {
        current_commit == Some(self.commit.as_str())
    }
}

/// The linked worktrees of a repository, sorted by their pull requests.
#[derive(Debug, Default)]
pub struct Check {
    /// The worktrees of the finished pull requests.
    pub finished: Vec<FinishedWorktree>,
    /// The worktrees whose branch has no pull request.
    pub without_pull_request: Vec<LinkedWorktree>,
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

/// Return the finished pull request that contains the local commit of a
/// branch, or None if the worktree must stay.
///
/// - `local_commit`: the commit of the local branch.
/// - `pull_requests`: the pull requests of the branch, in all the states.
/// - `default_branch`: where the local commit is. If the default branch is
///   not known, only the first condition applies.
/// - `head_contains`: tells if a pull request head contains the local
///   commit (a call to the forge).
pub fn finished_pull_request<'a>(
    local_commit: &str,
    pull_requests: &'a [PullRequest],
    default_branch: DefaultBranch,
    mut head_contains: impl FnMut(&str) -> bool,
) -> Option<&'a PullRequest> {
    let is_open = pull_requests
        .iter()
        .any(|pull_request| pull_request.state == PullRequestState::Open);
    if is_open || default_branch == DefaultBranch::Contains {
        return None;
    }
    let mut finished = pull_requests
        .iter()
        .filter(|pull_request| pull_request.state != PullRequestState::Open);
    if let Some(same_commit) = finished
        .clone()
        .find(|pull_request| pull_request.head == local_commit)
    {
        return Some(same_commit);
    }
    if default_branch == DefaultBranch::Unknown {
        return None;
    }
    finished.find(|pull_request| head_contains(&pull_request.head))
}

/// The backend of the forge of `main_checkout`, for the observer.
fn repository_backend(main_checkout: &Path) -> Result<Box<dyn Backend>, Error> {
    forge::of_repository(main_checkout)
        .and_then(|forge| forge::backend(&forge, forge::BACKGROUND_TIME_LIMIT))
        .map_err(Error::Command)
}

/// The pull requests of a branch, as the observer and the cleaner see
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchStatus {
    /// The branch has no pull request.
    NoPullRequest,
    /// The branch has an open pull request, with this number.
    Open(u64),
    /// A finished pull request contains the local commit of the branch:
    /// the worktree is finished.
    Finished {
        pull_request: PullRequest,
        commit: String,
    },
    /// The pull requests of the branch are finished, but the worktree
    /// must stay. The number is the last pull request.
    Kept { number: u64, why: KeptWhy },
}

/// Why a worktree with finished pull requests stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeptWhy {
    /// The branch has a commit after its finished pull requests.
    WorkAfter,
    /// The default branch contains the commit of the branch: a new
    /// worktree starts there, or wt merged it without a pull request.
    /// `wt step prune` removes a merged worktree.
    InDefaultBranch,
    /// The local branch has no commit.
    NoLocalCommit,
}

/// Read the pull requests of `branch` (one call to the forge, sometimes 2),
/// and tell the status of its worktree. `default_branch` is the default
/// branch of `origin`, if git knows it.
pub fn branch_status(
    main_checkout: &Path,
    backend: &dyn Backend,
    default_branch: Option<&str>,
    branch: &str,
) -> Result<BranchStatus, Error> {
    let pull_requests = backend.pull_requests(branch)?;
    let Some(last) = pull_requests.first() else {
        return Ok(BranchStatus::NoPullRequest);
    };
    if let Some(open) = pull_requests
        .iter()
        .find(|pull_request| pull_request.state == PullRequestState::Open)
    {
        return Ok(BranchStatus::Open(open.number));
    }
    let Some(local_commit) = git::branch_commit(main_checkout, branch) else {
        return Ok(BranchStatus::Kept {
            number: last.number,
            why: KeptWhy::NoLocalCommit,
        });
    };
    let place = match default_branch {
        None => DefaultBranch::Unknown,
        Some(default) if git::is_ancestor(main_checkout, &local_commit, default) => {
            DefaultBranch::Contains
        }
        Some(_) => DefaultBranch::DoesNotContain,
    };
    let finished = finished_pull_request(&local_commit, &pull_requests, place, |head| {
        backend.contains(head, &local_commit)
    });
    Ok(match finished {
        Some(pull_request) => BranchStatus::Finished {
            pull_request: pull_request.clone(),
            commit: local_commit,
        },
        None => BranchStatus::Kept {
            number: last.number,
            why: if place == DefaultBranch::Contains {
                KeptWhy::InDefaultBranch
            } else {
                KeptWhy::WorkAfter
            },
        },
    })
}

/// Sort the linked worktrees of the repository of `main_checkout` by their
/// pull requests.
///
/// `skip` tells if the thread must keep a worktree without a question to
/// the forge, for example a worktree where an agent works. If the forge of
/// `origin` has no backend, or if git or the forge fails, the function
/// returns an error and no worktree.
pub fn check_worktrees(main_checkout: &Path, skip: impl Fn(&Path) -> bool) -> Result<Check, Error> {
    let worktrees: Vec<LinkedWorktree> = git::linked_worktrees(main_checkout)?
        .into_iter()
        .filter(|worktree| !skip(&worktree.path))
        .collect();
    if worktrees.is_empty() {
        return Ok(Check::default());
    }
    let backend = repository_backend(main_checkout)?;
    let default_branch = git::default_branch(main_checkout);
    let mut check = Check::default();
    for worktree in worktrees {
        match branch_status(
            main_checkout,
            backend.as_ref(),
            default_branch.as_deref(),
            &worktree.branch,
        )? {
            BranchStatus::NoPullRequest => check.without_pull_request.push(worktree),
            BranchStatus::Finished {
                pull_request,
                commit,
            } => check.finished.push(FinishedWorktree {
                recover_command: backend.recover_command(&pull_request, &commit, &worktree.branch),
                reference: backend.reference(pull_request.number),
                path: worktree.path,
                branch: worktree.branch,
                pull_request,
                commit,
            }),
            BranchStatus::Open(_) | BranchStatus::Kept { .. } => {}
        }
    }
    Ok(check)
}

/// Keep the old worktrees whose branch has no pull request on the forge.
///
/// The caller selects the old worktrees with local checks first (agents,
/// activity, changes): this function then calls the forge once for each
/// of them only. Without old worktrees, it calls nothing, also for a
/// repository without a forge.
pub fn without_pull_request(
    main_checkout: &Path,
    old: Vec<OldWorktree>,
) -> Result<Vec<OldWorktree>, Error> {
    if old.is_empty() {
        return Ok(old);
    }
    let backend = repository_backend(main_checkout)?;
    let mut kept = Vec::new();
    for worktree in old {
        if backend.pull_requests(&worktree.branch)?.is_empty() {
            kept.push(worktree);
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{git_in, git_output, test_folder};

    fn pull_request(number: u64, state: PullRequestState, head: &str) -> PullRequest {
        PullRequest {
            number,
            state,
            head: head.to_owned(),
        }
    }

    fn never(_head: &str) -> bool {
        panic!("GitHub must not be asked");
    }

    const MERGED: PullRequestState = PullRequestState::Merged;
    const CLOSED: PullRequestState = PullRequestState::Closed;
    const OPEN: PullRequestState = PullRequestState::Open;

    #[test]
    fn a_merged_pull_request_with_the_local_commit_is_finished() {
        let pull_requests = [
            pull_request(1, MERGED, "bbb"),
            pull_request(2, MERGED, "aaa"),
        ];

        let finished =
            finished_pull_request("aaa", &pull_requests, DefaultBranch::DoesNotContain, never);

        assert_eq!(finished.map(|pull_request| pull_request.number), Some(2));
    }

    #[test]
    fn a_closed_pull_request_that_contains_the_local_commit_is_finished() {
        let pull_requests = [pull_request(3, CLOSED, "bbb")];

        let finished = finished_pull_request(
            "aaa",
            &pull_requests,
            DefaultBranch::DoesNotContain,
            |head| head == "bbb",
        );

        assert_eq!(
            finished.map(|pull_request| pull_request.state),
            Some(CLOSED)
        );
    }

    #[test]
    fn an_open_pull_request_keeps_the_worktree() {
        let pull_requests = [pull_request(1, MERGED, "aaa"), pull_request(2, OPEN, "ccc")];

        let finished =
            finished_pull_request("aaa", &pull_requests, DefaultBranch::DoesNotContain, never);

        assert_eq!(finished, None);
    }

    #[test]
    fn a_local_commit_in_the_default_branch_keeps_the_worktree() {
        // A new worktree on the name of a branch that a fast-forward merged.
        let pull_requests = [pull_request(1, MERGED, "aaa")];

        let finished = finished_pull_request("aaa", &pull_requests, DefaultBranch::Contains, never);

        assert_eq!(finished, None);
    }

    #[test]
    fn a_pull_request_that_does_not_contain_the_local_commit_keeps_the_worktree() {
        let pull_requests = [pull_request(1, CLOSED, "bbb")];

        let finished =
            finished_pull_request("aaa", &pull_requests, DefaultBranch::DoesNotContain, |_| {
                false
            });

        assert_eq!(finished, None);
    }

    #[test]
    fn an_unknown_default_branch_accepts_only_the_same_commit() {
        let same = [pull_request(1, MERGED, "aaa")];
        let other = [pull_request(2, MERGED, "bbb")];

        assert!(finished_pull_request("aaa", &same, DefaultBranch::Unknown, never).is_some());
        assert!(finished_pull_request("aaa", &other, DefaultBranch::Unknown, never).is_none());
    }

    #[test]
    fn a_branch_without_pull_request_is_not_finished() {
        assert_eq!(
            finished_pull_request("aaa", &[], DefaultBranch::DoesNotContain, never),
            None
        );
    }

    fn finished_worktree() -> FinishedWorktree {
        FinishedWorktree {
            path: PathBuf::from("/work/repo.feature"),
            branch: "feature".to_owned(),
            commit: "aaa".to_owned(),
            pull_request: pull_request(1, MERGED, "aaa"),
            recover_command: "git fetch origin pull/1/head:feature".to_owned(),
            reference: "pull request #1".to_owned(),
        }
    }

    /// A forge for the tests: its pull requests, and the pairs (head,
    /// commit) where the head contains the commit.
    struct FakeForge {
        pull_requests: Vec<PullRequest>,
        contains: Vec<(String, String)>,
    }

    impl Backend for FakeForge {
        fn pull_requests(&self, _branch: &str) -> Result<Vec<PullRequest>, Error> {
            Ok(self.pull_requests.clone())
        }

        fn contains(&self, head: &str, commit: &str) -> bool {
            self.contains
                .iter()
                .any(|(known_head, known_commit)| known_head == head && known_commit == commit)
        }

        fn recover_command(
            &self,
            pull_request: &PullRequest,
            commit: &str,
            branch: &str,
        ) -> String {
            format!("fetch {} {commit} {branch}", pull_request.number)
        }

        fn reference(&self, number: u64) -> String {
            format!("request {number}")
        }
    }

    /// A repository with the branch `main` (one commit), the branch `work`
    /// (one more commit), and the branch `fresh` on the commit of `main`.
    /// Return its folder and the commit of `work`.
    fn repository_with_branches(name: &str) -> (PathBuf, String) {
        let folder = test_folder(name);
        git_in(&folder, &["init", "-q", "-b", "main"]);
        git_in(&folder, &["commit", "-q", "--allow-empty", "-m", "start"]);
        git_in(&folder, &["branch", "fresh"]);
        git_in(&folder, &["checkout", "-q", "-b", "work"]);
        git_in(&folder, &["commit", "-q", "--allow-empty", "-m", "work"]);
        let work = git_output(&folder, &["rev-parse", "work"]);
        git_in(&folder, &["checkout", "-q", "main"]);
        (folder, work)
    }

    fn status_of(
        folder: &Path,
        forge: &FakeForge,
        default_branch: Option<&str>,
        branch: &str,
    ) -> BranchStatus {
        branch_status(folder, forge, default_branch, branch).unwrap()
    }

    #[test]
    fn branch_status_reads_each_case_of_the_forge() {
        let (folder, work) = repository_with_branches("branch-status");
        let forge = |pull_requests: Vec<PullRequest>| FakeForge {
            pull_requests,
            contains: vec![("newer".to_owned(), work.clone())],
        };

        assert_eq!(
            status_of(&folder, &forge(vec![]), Some("main"), "work"),
            BranchStatus::NoPullRequest
        );
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![
                    pull_request(3, OPEN, "x"),
                    pull_request(2, MERGED, &work)
                ]),
                Some("main"),
                "work"
            ),
            BranchStatus::Open(3)
        );
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![pull_request(2, MERGED, &work)]),
                Some("main"),
                "work"
            ),
            BranchStatus::Finished {
                pull_request: pull_request(2, MERGED, &work),
                commit: work.clone(),
            }
        );
        // The button "Update branch" adds a commit on the forge only.
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![pull_request(2, CLOSED, "newer")]),
                Some("main"),
                "work"
            ),
            BranchStatus::Finished {
                pull_request: pull_request(2, CLOSED, "newer"),
                commit: work.clone(),
            }
        );
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![pull_request(2, MERGED, "older")]),
                Some("main"),
                "work"
            ),
            BranchStatus::Kept {
                number: 2,
                why: KeptWhy::WorkAfter
            }
        );
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![pull_request(2, MERGED, "older")]),
                Some("main"),
                "fresh"
            ),
            BranchStatus::Kept {
                number: 2,
                why: KeptWhy::InDefaultBranch
            }
        );
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![pull_request(2, MERGED, "older")]),
                Some("main"),
                "gone"
            ),
            BranchStatus::Kept {
                number: 2,
                why: KeptWhy::NoLocalCommit
            }
        );
        // Without a default branch, only the same commit proves the end.
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![pull_request(2, CLOSED, "newer")]),
                None,
                "work"
            ),
            BranchStatus::Kept {
                number: 2,
                why: KeptWhy::WorkAfter
            }
        );
    }

    #[test]
    fn a_branch_with_the_checked_commit_is_unchanged() {
        assert!(finished_worktree().is_unchanged(Some("aaa")));
    }

    #[test]
    fn a_branch_with_a_new_commit_is_changed() {
        assert!(!finished_worktree().is_unchanged(Some("bbb")));
    }

    #[test]
    fn a_deleted_branch_is_changed() {
        assert!(!finished_worktree().is_unchanged(None));
    }
}
