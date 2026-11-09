//! The forge of a repository: its provider, and the backend that reads the
//! pull requests of a branch.
//!
//! The provider comes from the host of the remote `origin`: a host with
//! github, gitlab, gitea, forgejo or codeberg in its name, or Azure DevOps.
//! A self-hosted host without such a name needs the forge config of
//! worktrunk: `[forge] platform` in the project config (`.config/wt.toml`),
//! or `forge.platform` in a `[projects."<host>/*"]` table of the user
//! config of worktrunk. The plugin reads both with
//! `wt config show --format json`.
//!
//! This version has a backend for GitHub only. The other providers give
//! "no backend": the cleaner keeps their worktrees, except the leftovers
//! that git proves.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::git;
use crate::github;
use crate::prune::{self, BranchStatus};

/// The URL schemes of a remote. A remote without scheme has the form
/// `[user@]host:owner/name`.
const SCHEMES: [&str; 4] = ["ssh://", "https://", "http://", "git://"];

/// The provider of a forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    GitLab,
    /// Gitea, Forgejo and Codeberg.
    Gitea,
    AzureDevOps,
    /// A host that the plugin does not know.
    Unknown,
}

impl Provider {
    /// The name of the provider, for the user.
    pub fn name(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::GitLab => "GitLab",
            Provider::Gitea => "Gitea",
            Provider::AzureDevOps => "Azure DevOps",
            Provider::Unknown => "an unknown forge",
        }
    }

    /// The provider of a `forge.platform` value of worktrunk.
    fn from_platform(platform: &str) -> Option<Provider> {
        match platform {
            "github" => Some(Provider::GitHub),
            "gitlab" => Some(Provider::GitLab),
            "gitea" | "forgejo" => Some(Provider::Gitea),
            "azure-devops" => Some(Provider::AzureDevOps),
            _ => None,
        }
    }

    /// The provider of a host, from its name, as worktrunk does.
    pub fn from_host(host: &str) -> Provider {
        let host = host.to_ascii_lowercase();
        if host.contains("github") {
            Provider::GitHub
        } else if host.contains("gitlab") {
            Provider::GitLab
        } else if ["gitea", "forgejo", "codeberg"]
            .iter()
            .any(|name| host.contains(name))
        {
            Provider::Gitea
        } else if host == "dev.azure.com"
            || host == "ssh.dev.azure.com"
            || host.ends_with(".visualstudio.com")
        {
            Provider::AzureDevOps
        } else {
            Provider::Unknown
        }
    }
}

/// The host and the path of a remote URL: `git@github.com:owner/name.git`
/// gives `github.com` and `owner/name.git`.
pub fn split_remote(url: &str) -> Option<(&str, &str)> {
    match SCHEMES.iter().find_map(|scheme| url.strip_prefix(scheme)) {
        // `[user@]host[:port]/owner/name`
        Some(rest) => {
            let (host_and_port, path) = without_user(rest).split_once('/')?;
            let host = host_and_port.split(':').next().unwrap_or_default();
            Some((host, path))
        }
        // `[user@]host:owner/name`
        None => without_user(url).split_once(':'),
    }
}

/// The address without its user: `git@github.com` gives `github.com`.
fn without_user(address: &str) -> &str {
    match address.split_once('@') {
        Some((_, rest)) => rest,
        None => address,
    }
}

/// The provider that the forge config of worktrunk gives, from the JSON
/// of `wt config show --format json`: the project config first, then the
/// most specific `[projects."…"]` table of the user config that matches
/// the identifier of the project (`<host>/<owner>/<repo>`).
pub fn configured_provider(config: &Value) -> Option<Provider> {
    let platform_of = |table: &Value| table["forge"]["platform"].as_str().map(str::to_owned);
    if let Some(platform) = platform_of(&config["project"]["config"]) {
        return Provider::from_platform(&platform);
    }
    let identifier = config["project"]["identifier"].as_str()?;
    let projects = config["user"]["config"]["projects"].as_object()?;
    projects
        .iter()
        .filter(|(pattern, _)| matches_identifier(pattern, identifier))
        // The most specific pattern wins: the most characters that are not *.
        .max_by_key(|(pattern, _)| pattern.chars().filter(|c| *c != '*').count())
        .and_then(|(_, table)| platform_of(table))
        .and_then(|platform| Provider::from_platform(&platform))
}

/// Return true if a key of `[projects."…"]` matches an identifier: the same
/// text, or a prefix that ends with `*`.
fn matches_identifier(pattern: &str, identifier: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => identifier.starts_with(prefix),
        None => pattern == identifier,
    }
}

/// The forge of a repository, for the checks of its branches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryForge {
    pub provider: Provider,
    /// The repository on the forge (`owner/name`), when the backend needs
    /// it.
    pub repository: Option<String>,
}

/// Find the forge of the repository of `main_checkout`. An error tells
/// why the cleaner cannot ask a forge (no remote `origin`).
pub fn detect(main_checkout: &Path) -> Result<RepositoryForge, String> {
    let url = git::origin_url(main_checkout).ok_or_else(|| "no remote origin".to_owned())?;
    let host = split_remote(&url)
        .map(|(host, _)| host.to_owned())
        .ok_or_else(|| format!("cannot read the remote {url}"))?;
    let provider = worktrunk_config(main_checkout)
        .as_ref()
        .and_then(configured_provider)
        .unwrap_or_else(|| Provider::from_host(&host));
    Ok(RepositoryForge {
        provider,
        repository: github::repository_of(&url),
    })
}

/// The JSON of `wt config show`, in the repository. None if wt fails.
fn worktrunk_config(main_checkout: &Path) -> Option<Value> {
    let output = Command::new("wt")
        .arg("-C")
        .arg(main_checkout)
        .args(["config", "show", "--format", "json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The status of the pull requests of `branch`, from the backend of the
/// provider. An error tells why the forge gave no answer: no backend, gh
/// failed, a time limit.
pub fn branch_status(
    forge: &RepositoryForge,
    main_checkout: &Path,
    default_branch: Option<&str>,
    branch: &str,
    limit: Duration,
) -> Result<BranchStatus, String> {
    match forge.provider {
        Provider::GitHub => {
            let repository = forge
                .repository
                .as_deref()
                .ok_or_else(|| "GitHub Enterprise has no backend yet".to_owned())?;
            prune::branch_status(main_checkout, repository, default_branch, branch, limit)
                .map_err(|error| error.to_string().replace('\n', " "))
        }
        provider => Err(format!("no backend for {} yet", provider.name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provider_comes_from_the_name_of_the_host() {
        let cases = [
            ("github.com", Provider::GitHub),
            ("github.example.com", Provider::GitHub),
            ("gitlab.com", Provider::GitLab),
            ("gitlab.company.example", Provider::GitLab),
            ("gitea.com", Provider::Gitea),
            ("codeberg.org", Provider::Gitea),
            ("forgejo.example.org", Provider::Gitea),
            ("dev.azure.com", Provider::AzureDevOps),
            ("ssh.dev.azure.com", Provider::AzureDevOps),
            ("team.visualstudio.com", Provider::AzureDevOps),
            ("git.company.example", Provider::Unknown),
        ];
        for (host, provider) in cases {
            assert_eq!(Provider::from_host(host), provider, "{host}");
        }
    }

    #[test]
    fn split_remote_reads_each_form_of_url() {
        let cases = [
            (
                "git@github.com:owner/name.git",
                ("github.com", "owner/name.git"),
            ),
            (
                "https://gitlab.com/group/sub/name",
                ("gitlab.com", "group/sub/name"),
            ),
            (
                "ssh://git@git.example:2222/team/app.git",
                ("git.example", "team/app.git"),
            ),
            (
                "https://user@codeberg.org/owner/name",
                ("codeberg.org", "owner/name"),
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(split_remote(url), Some(expected), "{url}");
        }
        assert_eq!(split_remote("/local/path"), None);
    }

    fn config(project: Value, projects: Value) -> Value {
        serde_json::json!({
            "project": { "identifier": "git.company.example/platform/app", "config": project },
            "user": { "config": { "projects": projects } },
        })
    }

    #[test]
    fn the_config_of_worktrunk_gives_the_provider_of_a_self_hosted_forge() {
        let projects = serde_json::json!({
            "git.company.example/*": { "forge": { "platform": "gitlab" } },
            "git.company.example/platform/*": { "forge": { "platform": "gitea" } },
            "other.example/*": { "forge": { "platform": "github" } },
        });

        // The most specific table wins.
        assert_eq!(
            configured_provider(&config(Value::Null, projects.clone())),
            Some(Provider::Gitea)
        );
        // The project config wins over the user config.
        let project = serde_json::json!({ "forge": { "platform": "azure-devops" } });
        assert_eq!(
            configured_provider(&config(project, projects)),
            Some(Provider::AzureDevOps)
        );
        // No table: the host decides.
        assert_eq!(
            configured_provider(&config(Value::Null, serde_json::json!({}))),
            None
        );
    }

    #[test]
    fn a_provider_without_backend_gives_an_error() {
        let forge = RepositoryForge {
            provider: Provider::GitLab,
            repository: None,
        };

        let status = branch_status(
            &forge,
            Path::new("/work/app"),
            None,
            "feat",
            Duration::from_secs(1),
        );

        assert_eq!(status, Err("no backend for GitLab yet".to_owned()));
    }
}
