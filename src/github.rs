//! The calls to GitHub, with the gh CLI: the removal of the finished
//! worktrees (`prune_merged`) and the checks of the popup cleaner.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::forge::{
    self, Backend, ForgeError, Provider, PullRequest, PullRequestState, RepositoryForge, PAGE_SIZE,
};
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
    /// Run gh without a terminal, and return its output. The arguments
    /// name the repository: gh reads no remote. gh runs in the folder of the
    /// process, where a shim of mise finds its version.
    fn gh(&self, arguments: &[&str]) -> Result<String, Error> {
        let label = format!(
            "gh {}",
            arguments
                .iter()
                .take(2)
                .copied()
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut command = Command::new("gh");
        command.args(arguments).env("GH_PROMPT_DISABLED", "1");
        process::run_cli(&mut command, &label, self.limit)
    }

    pub fn new(repository: String, limit: Duration) -> GitHub {
        GitHub { repository, limit }
    }

    /// The backend of `forge`: github.com only, with an owner and a name.
    pub fn from_forge(forge: &RepositoryForge, limit: Duration) -> Result<GitHub, ForgeError> {
        let host = forge::api_host(forge, "github.com")?;
        if host != "github.com" {
            return Err(ForgeError::Unsupported(format!(
                "GitHub on {host} has no backend yet"
            )));
        }
        Ok(GitHub::new(forge::owner_and_name(forge)?.to_owned(), limit))
    }
}

impl Backend for GitHub {
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error> {
        let text = self.gh(&[
            "pr",
            "list",
            "--repo",
            &self.repository,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            &PAGE_SIZE.to_string(),
            "--json",
            "number,state,headRefOid,isCrossRepository",
        ])?;
        parse_pull_requests(&text)
    }

    /// GitHub compares `commit...head`: the status `ahead` tells that
    /// `head` contains `commit`. If GitHub does not know `commit` (it is
    /// not pushed), the call fails and the function returns false.
    fn contains(&self, head: &str, commit: &str) -> bool {
        let endpoint = format!("repos/{}/compare/{commit}...{head}", self.repository);
        self.gh(&["api", &endpoint, "--jq", ".status"])
            .is_ok_and(|status| status.trim() == "ahead")
    }

    /// GitHub keeps the commits of a closed pull request in
    /// `refs/pull/<number>/head`, without a time limit.
    fn recover_command(&self, pull_request: &PullRequest, _commit: &str, branch: &str) -> String {
        format!(
            "git fetch origin pull/{}/head:{branch}",
            pull_request.number
        )
    }

    fn provider(&self) -> Provider {
        Provider::GitHub
    }
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
    let listed: Vec<Listed> = forge::parse_page(text, "gh pr list")?;
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

    #[test]
    fn a_full_page_gives_an_error() {
        let page = crate::test_support::full_page(|number| {
            format!(r#"{{"headRefOid":"aaa","number":{number},"state":"CLOSED"}}"#)
        });

        assert!(parse_pull_requests(&page).is_err());
    }
}
