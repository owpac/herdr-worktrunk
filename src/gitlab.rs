//! The calls to GitLab, with the glab CLI: `glab api` on the REST API v4.
//!
//! glab keeps the login of each host (`glab auth login --hostname <host>`).
//! A public project on gitlab.com needs no login.
//!
//! - The merge requests of a branch: `projects/<project>/merge_requests`
//!   with `source_branch` and `state=all`. A merge request from a fork has
//!   another `source_project_id`: it does not count.
//! - The proof that a merge request contains a commit:
//!   `projects/<project>/repository/merge_base` of the commit and of the
//!   head. The merge base is the commit itself if the head contains it.

use std::env;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::forge::{
    self, Backend, ForgeError, Provider, PullRequest, PullRequestState, RepositoryForge, PAGE_SIZE,
};
use crate::herdr::Error;
use crate::process;
use crate::url::encode_part as encode;

/// The backend of a project on a GitLab host.
pub struct GitLab {
    /// The host of the API, for example `gitlab.com`.
    host: String,
    /// The path of the project: `group/name` or `group/subgroup/name`.
    project: String,
    /// The time limit of each call to glab.
    limit: Duration,
}

impl GitLab {
    /// The backend of `forge`: its host of the API, and the path of the
    /// project.
    pub fn from_forge(forge: &RepositoryForge, limit: Duration) -> Result<GitLab, ForgeError> {
        Ok(GitLab::new(
            forge::api_host(forge, "gitlab.com")?,
            forge::repository_path(forge)?.to_owned(),
            limit,
        ))
    }

    pub fn new(host: String, project: String, limit: Duration) -> GitLab {
        GitLab {
            host,
            project,
            limit,
        }
    }

    /// Run `glab api` on `endpoint` without a terminal, and return its
    /// output.
    fn api(&self, endpoint: &str) -> Result<String, Error> {
        let mut command = Command::new("glab");
        command
            .args(["api", "--hostname", &self.host, endpoint])
            .env("GLAB_NO_PROMPT", "1")
            .env("GLAB_CHECK_UPDATE", "false")
            .env("NO_COLOR", "1")
            // Not in a repository: glab must not read the remotes of a
            // checkout.
            .current_dir(env::temp_dir());
        process::run_cli(&mut command, "glab api", self.limit)
    }

    /// The path of the API of the project, with its encoded path.
    fn project_endpoint(&self) -> String {
        format!("projects/{}", encode(&self.project))
    }
}

impl Backend for GitLab {
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error> {
        let endpoint = format!(
            "{}/merge_requests?source_branch={}&state=all&per_page={PAGE_SIZE}",
            self.project_endpoint(),
            encode(branch)
        );
        parse_merge_requests(&self.api(&endpoint)?)
    }

    /// GitLab does not know a commit that was never pushed: the call then
    /// fails, and the function returns false.
    fn contains(&self, head: &str, commit: &str) -> bool {
        let endpoint = format!(
            "{}/repository/merge_base?refs[]={}&refs[]={}",
            self.project_endpoint(),
            encode(commit),
            encode(head)
        );
        self.api(&endpoint)
            .is_ok_and(|text| merge_base_is(&text, commit))
    }

    /// GitLab deletes `refs/merge-requests/<iid>/head` 14 days after the
    /// end of a merge request, but keeps its commits: a fetch of the
    /// commit itself works.
    fn recover_command(&self, _pull_request: &PullRequest, commit: &str, branch: &str) -> String {
        format!("git fetch origin {commit}:refs/heads/{branch}")
    }

    fn provider(&self) -> Provider {
        Provider::GitLab
    }
}

/// Read the JSON of the merge requests of a branch. The merge requests
/// from a fork do not count. A state that the plugin does not know keeps
/// the worktree, as an open merge request.
fn parse_merge_requests(text: &str) -> Result<Vec<PullRequest>, Error> {
    #[derive(Deserialize)]
    struct Listed {
        iid: u64,
        state: String,
        /// The last commit. GitLab can give null for a merge request
        /// without commit.
        sha: Option<String>,
        project_id: u64,
        source_project_id: u64,
    }
    let listed: Vec<Listed> = forge::parse_page(text, "glab api")?;
    Ok(listed
        .into_iter()
        .filter(|listed| listed.source_project_id == listed.project_id)
        .map(|listed| PullRequest {
            number: listed.iid,
            state: match listed.state.as_str() {
                "merged" => PullRequestState::Merged,
                "closed" => PullRequestState::Closed,
                // "opened", "locked" (GitLab merges it now) and the
                // unknown states.
                _ => PullRequestState::Open,
            },
            head: listed.sha.unwrap_or_default(),
        })
        .collect())
}

/// Return true if the JSON of a merge base gives the commit `commit`.
fn merge_base_is(text: &str, commit: &str) -> bool {
    #[derive(Deserialize)]
    struct MergeBase {
        id: String,
    }
    serde_json::from_str::<MergeBase>(text).is_ok_and(|base| base.id == commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_json_of_gitlab_gives_the_merge_requests() {
        let text = include_str!("../tests/fixtures/gitlab-merge-requests.json");

        let merge_requests = parse_merge_requests(text).unwrap();

        assert_eq!(
            merge_requests,
            [
                PullRequest {
                    number: 4364,
                    state: PullRequestState::Open,
                    head: "839348a67ae52e27c05033720582a5d88544b66a".to_owned(),
                },
                PullRequest {
                    number: 4362,
                    state: PullRequestState::Merged,
                    head: "255cd9aa6f86bf8b5c8e472dea85c934c76d3fef".to_owned(),
                },
                PullRequest {
                    number: 4359,
                    state: PullRequestState::Closed,
                    head: "10c0df6256349e508f5e58e7afbb2698af484f67".to_owned(),
                },
                PullRequest {
                    number: 4358,
                    state: PullRequestState::Open,
                    head: "5a1c0be2a3f4e6d7c8b9a0f1e2d3c4b5a6978877".to_owned(),
                },
                PullRequest {
                    number: 4357,
                    state: PullRequestState::Open,
                    head: String::new(),
                },
            ]
        );
    }

    #[test]
    fn invalid_json_gives_an_error() {
        assert!(parse_merge_requests("<html>").is_err());
    }

    #[test]
    fn the_merge_base_must_be_the_commit() {
        let commit = "255cd9aa6f86bf8b5c8e472dea85c934c76d3fef";

        assert!(merge_base_is(&format!(r#"{{"id":"{commit}"}}"#), commit));
        assert!(!merge_base_is(
            r#"{"id":"0007f369ec584c9e5312255663d121e7fcd5abe5"}"#,
            commit
        ));
        assert!(!merge_base_is(
            r#"{"message":"Could not find ref"}"#,
            commit
        ));
        assert!(!merge_base_is("", commit));
    }

    fn gitlab() -> GitLab {
        GitLab::new(
            "gitlab.com".to_owned(),
            "team/app".to_owned(),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn the_branch_of_a_closed_merge_request_comes_back_with_its_commit() {
        let closed = PullRequest {
            number: 12,
            state: PullRequestState::Closed,
            head: "bbb".to_owned(),
        };

        assert_eq!(
            gitlab().recover_command(&closed, "aaa", "feat/x"),
            "git fetch origin aaa:refs/heads/feat/x"
        );
        assert_eq!(gitlab().provider().reference(12), "merge request !12");
    }

    #[test]
    fn a_full_page_gives_an_error() {
        let page = crate::test_support::full_page(|number| {
            format!(
                r#"{{"iid":{number},"state":"closed","sha":"aaa","project_id":1,"source_project_id":1}}"#
            )
        });

        assert!(parse_merge_requests(&page).is_err());
    }
}
