//! The calls to GitHub, with the gh CLI. Only the removal of the merged
//! worktrees (`prune_merged`) uses them.

use std::process::Command;
use std::time::Duration;

use crate::herdr::Error;
use crate::process;

/// The time limit of a gh command.
const GH_TIME_LIMIT: Duration = Duration::from_secs(60);

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
fn run(arguments: &[&str]) -> Result<String, Error> {
    let label = arguments
        .iter()
        .take(2)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new("gh");
    command.args(arguments).env("GH_PROMPT_DISABLED", "1");
    let output = process::output_with_limit(&mut command, GH_TIME_LIMIT)
        .map_err(|error| Error::Command(format!("gh {label}: {error}")))?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Command(format!("gh {label}: {}", details.trim())));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Return the last commit of each merged pull request of `branch`. The
/// list is empty if the branch has no merged pull request.
pub fn merged_heads(repository: &str, branch: &str) -> Result<Vec<String>, Error> {
    let text = run(&[
        "pr",
        "list",
        "--repo",
        repository,
        "--head",
        branch,
        "--state",
        "merged",
        "--json",
        "headRefOid",
        "--jq",
        ".[].headRefOid",
    ])?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Return true if the commit `head` contains the commit `commit`, as
/// GitHub sees them.
///
/// GitHub compares `commit...head`: the status `ahead` tells that `head`
/// contains `commit`. If GitHub does not know `commit` (it is not
/// pushed), the call fails and the function returns false.
pub fn contains(repository: &str, head: &str, commit: &str) -> bool {
    let endpoint = format!("repos/{repository}/compare/{commit}...{head}");
    run(&["api", &endpoint, "--jq", ".status"]).is_ok_and(|status| status.trim() == "ahead")
}

#[cfg(test)]
mod tests {
    use super::*;

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
