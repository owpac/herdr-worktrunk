//! The calls to GitHub, with the gh CLI: the removal of the finished
//! worktrees (`prune_merged`) and the checks of the popup cleaner.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::herdr::Error;
use crate::process;

/// The time limit of a gh command of the observer, in the background.
pub const GH_TIME_LIMIT: Duration = Duration::from_secs(60);
/// The time limit of a gh command of a popup: the user waits for it.
pub const POPUP_TIME_LIMIT: Duration = Duration::from_secs(15);

/// The URL schemes of a remote. A remote without scheme has the form
/// `[user@]host:owner/name`.
const SCHEMES: [&str; 4] = ["ssh://", "https://", "http://", "git://"];

/// Return the GitHub repository (`owner/name`) of a remote URL, or None if
/// the host of the remote is not `github.com`. An ssh alias of the host
/// (`Host` in `~/.ssh/config`) gives None.
pub fn repository_of(url: &str) -> Option<String> {
    let (host, path) = match SCHEMES.iter().find_map(|scheme| url.strip_prefix(scheme)) {
        // `[user@]host[:port]/owner/name`
        Some(rest) => {
            let (host_and_port, path) = without_user(rest).split_once('/')?;
            let host = host_and_port.split(':').next().unwrap_or_default();
            (host, path)
        }
        // `[user@]host:owner/name`
        None => without_user(url).split_once(':')?,
    };
    if host != "github.com" {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(path.to_owned())
}

/// Remove the user (`user@`) at the start of a remote address.
fn without_user(address: &str) -> &str {
    let host_end = address.find(['/', ':']).unwrap_or(address.len());
    match address[..host_end].rfind('@') {
        Some(at) => &address[at + 1..],
        None => address,
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

/// The state of a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PullRequestState {
    Open,
    Merged,
    Closed,
}

/// A pull request of a branch.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub state: PullRequestState,
    /// The last commit of the pull request.
    #[serde(rename = "headRefOid")]
    pub head: String,
}

/// Return the pull requests of `branch` of the repository itself, in all
/// the states. The list is empty if the branch has no pull request.
pub fn pull_requests(
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
    struct Listed {
        #[serde(flatten)]
        pull_request: PullRequest,
        #[serde(rename = "isCrossRepository", default)]
        is_cross_repository: bool,
    }
    let listed: Vec<Listed> = serde_json::from_str(text)
        .map_err(|error| Error::Command(format!("gh pr list: invalid JSON: {error}")))?;
    Ok(listed
        .into_iter()
        .filter(|listed| !listed.is_cross_repository)
        .map(|listed| listed.pull_request)
        .collect())
}

/// Return true if the commit `head` contains the commit `commit`, as
/// GitHub sees them.
///
/// GitHub compares `commit...head`: the status `ahead` tells that `head`
/// contains `commit`. If GitHub does not know `commit` (it is not
/// pushed), the call fails and the function returns false.
pub fn contains(repository: &str, head: &str, commit: &str, limit: Duration) -> bool {
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

    #[test]
    fn the_forms_of_a_github_url_give_the_repository() {
        for url in [
            "git@github.com:owpac/herdr-worktrunk.git",
            "github.com:owpac/herdr-worktrunk",
            "ssh://git@github.com/owpac/herdr-worktrunk.git",
            "ssh://git@github.com:22/owpac/herdr-worktrunk.git",
            "https://github.com/owpac/herdr-worktrunk",
            "https://user@github.com/owpac/herdr-worktrunk.git/",
            "git://github.com/owpac/herdr-worktrunk.git",
        ] {
            assert_eq!(
                repository_of(url).as_deref(),
                Some("owpac/herdr-worktrunk"),
                "{url}"
            );
        }
    }

    #[test]
    fn a_numeric_owner_is_not_a_port() {
        assert_eq!(
            repository_of("git@github.com:1234/repo.git").as_deref(),
            Some("1234/repo")
        );
    }

    #[test]
    fn a_remote_outside_github_has_no_repository() {
        for url in [
            "git@gitlab.com:owpac/prompts.git",
            "git@github-work:owpac/prompts.git",
            "https://github.example.com/owpac/prompts",
            "/work/bare-repo.git",
            "file:///work/bare-repo.git",
            "https://github.com/owpac",
        ] {
            assert_eq!(repository_of(url), None, "{url}");
        }
    }
}
