//! The calls to GitHub, with the gh CLI. Only the removal of the merged
//! worktrees (`prune_merged`) uses them.

use std::process::{Command, Stdio};

use crate::herdr::Error;

/// The prefixes of a GitHub remote URL, before `owner/name`.
const GITHUB_PREFIXES: [&str; 3] = [
    "git@github.com:",
    "ssh://git@github.com/",
    "https://github.com/",
];

/// Return the GitHub repository (`owner/name`) of a remote URL, or None if
/// the remote is not on GitHub.
pub fn repository_of(url: &str) -> Option<String> {
    let path = GITHUB_PREFIXES
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(path.to_owned())
}

/// Run gh without a terminal, and return its output.
fn run(arguments: &[&str]) -> Result<String, Error> {
    let output = Command::new("gh")
        .args(arguments)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::Command(format!("cannot run gh: {error}")))?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Command(format!(
            "gh {}: {}",
            arguments
                .iter()
                .take(2)
                .copied()
                .collect::<Vec<_>>()
                .join(" "),
            details.trim()
        )));
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
    fn the_3_forms_of_a_github_url_give_the_repository() {
        for url in [
            "git@github.com:owpac/herdr-worktrunk.git",
            "ssh://git@github.com/owpac/herdr-worktrunk.git",
            "https://github.com/owpac/herdr-worktrunk",
            "https://github.com/owpac/herdr-worktrunk.git/",
        ] {
            assert_eq!(
                repository_of(url).as_deref(),
                Some("owpac/herdr-worktrunk"),
                "{url}"
            );
        }
    }

    #[test]
    fn a_remote_outside_github_has_no_repository() {
        assert_eq!(repository_of("git@gitlab.com:owpac/prompts.git"), None);
        assert_eq!(repository_of("/work/bare-repo.git"), None);
        assert_eq!(repository_of("https://github.com/owpac"), None);
    }
}
