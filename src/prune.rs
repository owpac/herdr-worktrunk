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
//! Each backend gives the command that gets the branch back
//! (`Backend::recover_command`): GitHub, Gitea and Forgejo keep
//! `refs/pull/<number>/head`, GitLab and Azure DevOps give the commit
//! itself.
//!
//! A local commit in the default branch does not prove a finished pull
//! request: a new worktree starts there, also on the name of an old
//! branch. The check gives these worktrees in `Check::merged`, as the
//! worktrees of a merge without pull request (`wt merge --no-remove`) on a
//! branch that was never pushed. Then the rule `leftover::merged_branch`
//! decides: the reflog tells if the branch has its own commits.
//!
//! The observer does not fetch `origin`. Thus the local copy of the default
//! branch of `origin` can be old. An old copy contains fewer commits, so
//! the check can only remove a worktree of a finished pull request.
//!
//! A repository without the remote `origin` has no pull request: all its
//! worktrees are without pull request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge::{self, Backend, ForgeError, PullRequest, PullRequestState};
use crate::git::{self, LinkedWorktree};
use crate::herdr::Error;
use crate::leftover::{self, Push};
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

/// A worktree whose branch the default branch contains, without an open
/// pull request: after a merge commit, a merge without pull request, or a
/// new worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// The commit of the branch that the default branch contains.
    pub commit: String,
    /// The default branch that contains the commit: `main` or
    /// `origin/main`.
    pub default_branch: String,
    /// The branch has a finished pull request: its commit is in the default
    /// branch, without a proof that the pull request contains it.
    pub has_pull_request: bool,
}

/// The linked worktrees of a repository, sorted by their pull requests.
#[derive(Debug, Default)]
pub struct Check {
    /// The worktrees of the finished pull requests.
    pub finished: Vec<FinishedWorktree>,
    /// The worktrees whose branch is in the default branch, without an
    /// open pull request or a proof of a finished one.
    pub merged: Vec<MergedWorktree>,
    /// The other worktrees whose branch has no pull request.
    pub without_pull_request: Vec<LinkedWorktree>,
    /// The branches whose check failed, with the error: they stay. An
    /// error for one branch does not stop the check of the others.
    pub errors: Vec<(String, Error)>,
    /// The number of branches with an answer of the forge, or without a
    /// forge.
    pub checked: usize,
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
    // An empty head (a pull request without commit) proves nothing.
    let mut finished = pull_requests.iter().filter(|pull_request| {
        pull_request.state != PullRequestState::Open && !pull_request.head.is_empty()
    });
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

/// The backend of the forge of `main_checkout`, for the observer, or None
/// for a repository without the remote `origin`: it has no pull request.
fn repository_backend(main_checkout: &Path) -> Result<Option<Box<dyn Backend>>, Error> {
    match forge::of_repository(main_checkout)
        .and_then(|forge| forge::backend(&forge, forge::BACKGROUND_TIME_LIMIT))
    {
        Ok(backend) => Ok(Some(backend)),
        Err(ForgeError::NoRemote) => Ok(None),
        Err(error) => Err(Error::Command(error.to_string())),
    }
}

/// Return true if the rule of a merged branch applies to a branch with
/// this status of the forge and this push state: a finished pull request
/// whose commit is in the default branch, or no pull request and no push.
fn is_merge_candidate(status: &BranchStatus, push: Option<&Push>) -> bool {
    match status {
        BranchStatus::NoPullRequest => matches!(push, Some(Push::Never)),
        BranchStatus::Kept {
            why: KeptWhy::InDefaultBranch,
            ..
        } => true,
        _ => false,
    }
}

/// The default branches of a repository, local first: `main`, then
/// `origin/main`. Without network.
fn default_branches(main_checkout: &Path) -> Vec<String> {
    git::local_default_branch(main_checkout)
        .into_iter()
        .chain(git::default_branch(main_checkout))
        .collect()
}

/// The worktree of `worktree` as a merged worktree, if one of
/// `default_branches` contains the commit of its branch. A worktree on the
/// default branch itself is never merged.
fn merged_worktree(
    main_checkout: &Path,
    worktree: &LinkedWorktree,
    default_branches: &[String],
) -> Option<MergedWorktree> {
    if default_branches.first() == Some(&worktree.branch) {
        return None;
    }
    let commit = git::branch_commit(main_checkout, &worktree.branch)?;
    let default_branch = default_branches.iter().find(|default_branch| {
        let reference = match default_branch.strip_prefix("origin/") {
            Some(_) => format!("refs/remotes/{default_branch}"),
            None => format!("refs/heads/{default_branch}"),
        };
        git::is_ancestor(main_checkout, &commit, &reference)
    })?;
    Some(MergedWorktree {
        path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        commit,
        default_branch: default_branch.clone(),
        has_pull_request: false,
    })
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

/// Read the pull requests of `branch` (one or more calls to the forge),
/// and tell the status of its worktree. `origin_default_branch` is the default
/// branch of `origin`, if git knows it.
pub fn branch_status(
    main_checkout: &Path,
    backend: &dyn Backend,
    origin_default_branch: Option<&str>,
    branch: &str,
) -> Result<BranchStatus, Error> {
    let mut pull_requests = backend.pull_requests(branch)?;
    // The backends give no order. The newest pull request comes first:
    // the open one, the proof and the last number name the newest match.
    pull_requests.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.number));
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
    let place = match origin_default_branch {
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
/// A repository
/// without the remote `origin` has no pull request: its worktrees are all
/// without pull request. If the forge of `origin` has no backend, or if
/// git fails to list the worktrees, the function returns an error and no
/// worktree. An error of the forge for one branch goes to `Check::errors`.
///
/// A merged worktree (`Check::merged`) has a finished pull request, or a
/// branch that was never pushed: a local merge, for example `wt merge
/// --no-remove`. A pushed branch without pull request (`develop`) can be a
/// long work that the default branch receives from time to time: it stays
/// without pull request, for the daily question.
pub fn check_worktrees(main_checkout: &Path) -> Result<Check, Error> {
    let worktrees = git::linked_worktrees(main_checkout)?;
    if worktrees.is_empty() {
        return Ok(Check::default());
    }
    let backend = repository_backend(main_checkout)?;
    Ok(sort_worktrees(main_checkout, worktrees, backend.as_deref()))
}

/// Sort `worktrees` with the backend of the forge, or without a backend
/// for a repository without the remote `origin`.
fn sort_worktrees(
    main_checkout: &Path,
    worktrees: Vec<LinkedWorktree>,
    backend: Option<&dyn Backend>,
) -> Check {
    let origin_default_branch = git::default_branch(main_checkout);
    let default_branches = default_branches(main_checkout);
    let pushes = match (
        git::branch_upstreams(main_checkout),
        git::remote_branches(main_checkout),
    ) {
        (Some(branches), Some(remotes)) => leftover::parse_pushes(&branches, &remotes),
        _ => HashMap::new(),
    };
    let mut check = Check::default();
    for worktree in worktrees {
        let status = match backend {
            Some(backend) => branch_status(
                main_checkout,
                backend,
                origin_default_branch.as_deref(),
                &worktree.branch,
            ),
            None => Ok(BranchStatus::NoPullRequest),
        };
        let status = match status {
            Ok(status) => {
                check.checked += 1;
                status
            }
            Err(error) => {
                check.errors.push((worktree.branch, error));
                continue;
            }
        };
        if is_merge_candidate(&status, pushes.get(&worktree.branch)) {
            if let Some(mut merged) = merged_worktree(main_checkout, &worktree, &default_branches) {
                merged.has_pull_request = matches!(status, BranchStatus::Kept { .. });
                check.merged.push(merged);
                continue;
            }
        }
        match status {
            BranchStatus::NoPullRequest => check.without_pull_request.push(worktree),
            BranchStatus::Finished {
                pull_request,
                commit,
            } => {
                // A status of the forge needs a backend.
                let Some(backend) = backend else { continue };
                check.finished.push(FinishedWorktree {
                    recover_command: backend.recover_command(
                        &pull_request,
                        &commit,
                        &worktree.branch,
                    ),
                    reference: backend.provider().reference(pull_request.number),
                    path: worktree.path,
                    branch: worktree.branch,
                    pull_request,
                    commit,
                });
            }
            BranchStatus::Open(_) | BranchStatus::Kept { .. } => {}
        }
    }
    check
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
    let Some(backend) = repository_backend(main_checkout)? else {
        return Ok(old);
    };
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
    use crate::forge::Provider;
    use crate::test_support::{git_in, git_output, test_folder};

    fn pull_request(number: u64, state: PullRequestState, head: &str) -> PullRequest {
        PullRequest {
            number,
            state,
            head: head.to_owned(),
        }
    }

    fn never(_head: &str) -> bool {
        panic!("the forge must not be asked");
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

    /// A forge for the tests: its pull requests, and the pairs (head,
    /// commit) where the head contains the commit.
    struct FakeForge {
        pull_requests: Vec<PullRequest>,
        contains: Vec<(String, String)>,
    }

    impl Backend for FakeForge {
        fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error> {
            if branch == "broken" {
                return Err(Error::Command("gh pr list: HTTP 502".to_owned()));
            }
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

        fn provider(&self) -> Provider {
            Provider::GitHub
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
        origin_default_branch: Option<&str>,
        branch: &str,
    ) -> BranchStatus {
        branch_status(folder, forge, origin_default_branch, branch).unwrap()
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
        // The last pull request has the highest number, in any order.
        assert_eq!(
            status_of(
                &folder,
                &forge(vec![
                    pull_request(2, MERGED, "older"),
                    pull_request(9, CLOSED, "old")
                ]),
                Some("main"),
                "work"
            ),
            BranchStatus::Kept {
                number: 9,
                why: KeptWhy::WorkAfter
            }
        );
        // An empty head proves nothing, also when the local commit is empty.
        assert_eq!(
            finished_pull_request(
                "",
                &[pull_request(2, MERGED, "")],
                DefaultBranch::Unknown,
                never
            ),
            None
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
    fn a_repository_without_remote_gives_its_merged_worktrees() {
        let folder = test_folder("merged-without-remote");
        let repository = folder.join("app");
        git_in(&folder, &["init", "-q", "-b", "main", "app"]);
        git_in(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "start"],
        );
        git_in(&repository, &["config", "worktrunk.default-branch", "main"]);
        let worktree = |branch: &str| {
            let path = folder.join(format!("app.{branch}"));
            git_in(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
            path
        };
        // done: a commit, then a merge into main without pull request.
        let done = worktree("done");
        git_in(&done, &["commit", "-q", "--allow-empty", "-m", "done"]);
        git_in(&repository, &["merge", "-q", "--ff-only", "done"]);
        // fresh: a new worktree on the commit of main.
        worktree("fresh");
        // open: a commit that main does not have.
        let open = worktree("open");
        git_in(&open, &["commit", "-q", "--allow-empty", "-m", "open"]);

        let check = check_worktrees(&repository).unwrap();

        let merged: Vec<(&str, &str)> = check
            .merged
            .iter()
            .map(|worktree| (worktree.branch.as_str(), worktree.default_branch.as_str()))
            .collect();
        assert_eq!(merged, [("done", "main"), ("fresh", "main")]);
        assert!(check.finished.is_empty());
        let without: Vec<&str> = check
            .without_pull_request
            .iter()
            .map(|worktree| worktree.branch.as_str())
            .collect();
        assert_eq!(without, ["open"]);
        // The reflog tells the own commits of each merged branch.
        assert!(git::has_own_commits(&repository, "done"));
        assert!(!git::has_own_commits(&repository, "fresh"));
    }

    #[test]
    fn a_pushed_branch_without_pull_request_is_not_a_merged_worktree() {
        let kept = |why| BranchStatus::Kept { number: 4, why };
        let pushed = Push::Pushed("origin/develop".to_owned());

        assert!(is_merge_candidate(
            &BranchStatus::NoPullRequest,
            Some(&Push::Never)
        ));
        // develop: a long work that main receives from time to time.
        assert!(!is_merge_candidate(
            &BranchStatus::NoPullRequest,
            Some(&pushed)
        ));
        assert!(!is_merge_candidate(&BranchStatus::NoPullRequest, None));
        // A merge commit of a pull request: also a pushed branch.
        assert!(is_merge_candidate(
            &kept(KeptWhy::InDefaultBranch),
            Some(&pushed)
        ));
        assert!(!is_merge_candidate(
            &kept(KeptWhy::WorkAfter),
            Some(&Push::Never)
        ));
        assert!(!is_merge_candidate(
            &BranchStatus::Open(4),
            Some(&Push::Never)
        ));
    }

    #[test]
    fn an_error_of_the_forge_for_one_branch_does_not_stop_the_others() {
        let folder = test_folder("one-forge-error");
        let repository = folder.join("app");
        git_in(&folder, &["init", "-q", "-b", "main", "app"]);
        git_in(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "start"],
        );
        let worktree = |branch: &str| {
            let path = folder.join(format!("app.{branch}"));
            git_in(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
            git_in(&path, &["commit", "-q", "--allow-empty", "-m", branch]);
        };
        worktree("broken");
        worktree("other");
        let forge = FakeForge {
            pull_requests: vec![],
            contains: vec![],
        };

        let worktrees = git::linked_worktrees(&repository).unwrap();
        let check = sort_worktrees(&repository, worktrees, Some(&forge));

        let errors: Vec<&str> = check
            .errors
            .iter()
            .map(|(branch, _)| branch.as_str())
            .collect();
        assert_eq!(errors, ["broken"]);
        let without: Vec<&str> = check
            .without_pull_request
            .iter()
            .map(|worktree| worktree.branch.as_str())
            .collect();
        // The broken branch is not "without pull request": the daily
        // question does not see it.
        assert_eq!(without, ["other"]);
    }
}
