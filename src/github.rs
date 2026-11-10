//! The calls to GitHub, with the gh CLI: the removal of the finished
//! worktrees (`prune_merged`) and the checks of the popup cleaner.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::forge::{Backend, PullRequest, PullRequestState};
use crate::herdr::Error;
use crate::process;

/// The backend of a repository on github.com.
pub struct GitHub {
    /// The repository: `owner/name`.
    repository: String,
    /// The time limit of each call to gh.
    limit: Duration,
}

impl GitHub {
    pub fn new(repository: String, limit: Duration) -> GitHub {
        GitHub { repository, limit }
    }
}

impl Backend for GitHub {
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error> {
        pull_requests(&self.repository, branch, self.limit)
    }

    fn contains(&self, head: &str, commit: &str) -> bool {
        contains(&self.repository, head, commit, self.limit)
    }

    /// GitHub keeps the commits of a closed pull request in
    /// `refs/pull/<number>/head`.
    fn recover_command(&self, number: u64, branch: &str) -> String {
        format!("git fetch origin pull/{number}/head:{branch}")
    }
}

/// Run gh without a terminal, and return its output.
fn run(arguments: &[&str], limit: Duration) -> Result<String, Error> {
    let label = arguments
        .iter()
        .take(2)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new("gh");
    command.args(arguments).env("GH_PROMPT_DISABLED", "1");
    let output = process::output_with_limit(&mut command, limit)
        .map_err(|error| Error::Command(format!("gh {label}: {error}")))?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Command(format!("gh {label}: {}", details.trim())));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Return the pull requests of `branch` of the repository itself, in all
/// the states. The list is empty if the branch has no pull request.
fn pull_requests(
    repository: &str,
    branch: &str,
    limit: Duration,
) -> Result<Vec<PullRequest>, Error> {
    let text = run(
        &[
            "pr",
            "list",
            "--repo",
            repository,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "50",
            "--json",
            "number,state,headRefOid,isCrossRepository",
        ],
        limit,
    )?;
    parse_pull_requests(&text)
}

/// Read the JSON of `gh pr list`. `--head` gives also the pull requests of
/// a fork with a branch of the same name: the function ignores them.
fn parse_pull_requests(text: &str) -> Result<Vec<PullRequest>, Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "UPPERCASE")]
    enum State {
        Open,
        Merged,
        Closed,
    }
    #[derive(Deserialize)]
    struct Listed {
        number: u64,
        state: State,
        #[serde(rename = "headRefOid")]
        head: String,
        #[serde(rename = "isCrossRepository", default)]
        is_cross_repository: bool,
    }
    let listed: Vec<Listed> = serde_json::from_str(text)
        .map_err(|error| Error::Command(format!("gh pr list: invalid JSON: {error}")))?;
    Ok(listed
        .into_iter()
        .filter(|listed| !listed.is_cross_repository)
        .map(|listed| PullRequest {
            number: listed.number,
            state: match listed.state {
                State::Open => PullRequestState::Open,
                State::Merged => PullRequestState::Merged,
                State::Closed => PullRequestState::Closed,
            },
            head: listed.head,
        })
        .collect())
}

/// Return true if the commit `head` contains the commit `commit`, as
/// GitHub sees them.
///
/// GitHub compares `commit...head`: the status `ahead` tells that `head`
/// contains `commit`. If GitHub does not know `commit` (it is not
/// pushed), the call fails and the function returns false.
fn contains(repository: &str, head: &str, commit: &str, limit: Duration) -> bool {
    let endpoint = format!("repos/{repository}/compare/{commit}...{head}");
    run(&["api", &endpoint, "--jq", ".status"], limit).is_ok_and(|status| status.trim() == "ahead")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_json_of_gh_gives_the_pull_requests() {
        let text = r#"[{"headRefOid":"3541f65","number":4,"state":"MERGED","isCrossRepository":false},
            {"headRefOid":"aaa","number":7,"state":"OPEN","isCrossRepository":false},
            {"headRefOid":"fff","number":8,"state":"OPEN","isCrossRepository":true},
            {"headRefOid":"bbb","number":9,"state":"CLOSED","isCrossRepository":false}]"#;

        let pull_requests = parse_pull_requests(text).unwrap();

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.state))
                .collect::<Vec<_>>(),
            [
                (4, PullRequestState::Merged),
                (7, PullRequestState::Open),
                (9, PullRequestState::Closed)
            ]
        );
        assert_eq!(pull_requests[0].head, "3541f65");
    }
}
