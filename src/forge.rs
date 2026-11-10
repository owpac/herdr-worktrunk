//! The forge of a repository: its provider, and the backend that reads the
//! pull requests of a branch.
//!
//! worktrunk knows the forge: `wt list --format json` gives `repo.forge`
//! (provider, host, owner, name), with the forge config of the user
//! (`forge.platform` for a self-hosted host). Without wt, the provider
//! comes from the name of the host of `origin`.
//!
//! A backend reads the pull requests of one provider. The providers
//! without backend give "no backend": the cleaner keeps their worktrees,
//! except the leftovers that git proves.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::git;
use crate::github::GitHub;
use crate::herdr::Error;
use crate::process;
use crate::prune::{self, BranchStatus};

/// The URL schemes of a remote. A remote without scheme has the form
/// `[user@]host:owner/name`.
const SCHEMES: [&str; 4] = ["ssh://", "https://", "http://", "git://"];
/// The time limit of `ssh -G`, which reads only the config of ssh.
const SSH_CONFIG_TIME_LIMIT: Duration = Duration::from_secs(5);
/// The time limit of a call to a forge from the observer, in the
/// background.
pub const BACKGROUND_TIME_LIMIT: Duration = Duration::from_secs(60);
/// The time limit of a call to a forge from a popup: the user waits for
/// it.
pub const POPUP_TIME_LIMIT: Duration = Duration::from_secs(15);

/// The state of a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Merged,
    Closed,
}

/// A pull request of a branch: a merge request on GitLab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub state: PullRequestState,
    /// The last commit of the pull request.
    pub head: String,
}

/// The calls to one forge, for one repository.
pub trait Backend {
    /// The pull requests of `branch` in the repository itself, in all the
    /// states, the newest first. A pull request from a fork does not count.
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error>;

    /// Return true if the commit `head` contains the commit `commit`, as
    /// the forge sees them. False if the forge does not know `commit`.
    fn contains(&self, head: &str, commit: &str) -> bool;

    /// The command that gets back the branch of a closed pull request.
    fn recover_command(&self, number: u64, branch: &str) -> String;
}

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

    /// The provider of a `provider` value of worktrunk.
    fn from_worktrunk(provider: &str) -> Provider {
        match provider {
            "github" => Provider::GitHub,
            "gitlab" => Provider::GitLab,
            "gitea" | "forgejo" => Provider::Gitea,
            "azure-devops" => Provider::AzureDevOps,
            _ => Provider::Unknown,
        }
    }

    /// The provider of a host, from its name: the rule of worktrunk, and
    /// the hosts of Forgejo and Codeberg.
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

/// The address without its user: `git@github.com:a/b` gives
/// `github.com:a/b`. Only the host part can have a user: a `@` in the path
/// stays.
fn without_user(address: &str) -> &str {
    let host_end = address.find(['/', ':']).unwrap_or(address.len());
    match address[..host_end].rfind('@') {
        Some(at) => &address[at + 1..],
        None => address,
    }
}

/// The forge of a repository, for the checks of its branches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryForge {
    pub provider: Provider,
    /// The host of the forge, after the ssh aliases.
    pub host: String,
    /// The repository on the forge: `owner/name`.
    pub repository: Option<String>,
}

/// The forge that `wt list --format json` gives in `repo.forge`.
pub fn from_worktrunk(json: &Value) -> Option<RepositoryForge> {
    let forge = &json["repo"]["forge"];
    let provider = Provider::from_worktrunk(forge["provider"].as_str()?);
    let host = forge["host"].as_str().unwrap_or_default().to_owned();
    let repository = match (forge["owner"].as_str(), forge["name"].as_str()) {
        (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() => {
            Some(format!("{owner}/{name}"))
        }
        _ => None,
    };
    Some(RepositoryForge {
        provider,
        host,
        repository,
    })
}

/// The forge of the remote `origin`, without wt: the provider from the
/// name of the host. An error tells why the checks cannot ask a forge.
pub fn from_origin(main_checkout: &Path) -> Result<RepositoryForge, String> {
    let url = git::origin_url(main_checkout).ok_or_else(|| NO_REMOTE.to_owned())?;
    let (host, path) = split_remote(&url).ok_or_else(|| format!("cannot read the remote {url}"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    Ok(RepositoryForge {
        provider: Provider::from_host(host),
        host: host.to_owned(),
        repository: Some(path.to_owned()).filter(|path| path.contains('/')),
    })
}

/// The error of a repository without a remote `origin`: its branches can
/// have no pull request.
pub const NO_REMOTE: &str = "no remote origin";

/// The host behind an ssh alias (`Host github-work` in `~/.ssh/config`),
/// from `ssh -G`. The host itself if ssh does not know it.
fn real_host(host: &str) -> String {
    let mut command = Command::new("ssh");
    command.args(["-G", host]);
    process::output_with_limit(&mut command, SSH_CONFIG_TIME_LIMIT)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("hostname ").map(str::to_owned))
        })
        .unwrap_or_else(|| host.to_owned())
}

/// The backend of the provider of `forge`. An error tells why the plugin
/// cannot ask the forge: no backend, no repository.
pub fn backend(forge: &RepositoryForge, limit: Duration) -> Result<Box<dyn Backend>, String> {
    match forge.provider {
        Provider::GitHub => {
            let host = if forge.host == "github.com" {
                forge.host.clone()
            } else {
                real_host(&forge.host)
            };
            if host != "github.com" {
                return Err(format!("GitHub on {host} has no backend yet"));
            }
            let repository = forge
                .repository
                .clone()
                .ok_or_else(|| "no owner and name for the repository on GitHub".to_owned())?;
            Ok(Box::new(GitHub::new(repository, limit)))
        }
        provider => Err(format!("no backend for {} yet", provider.name())),
    }
}

/// The status of the pull requests of `branch`, from the backend of the
/// provider. An error tells why the forge gave no answer: no backend, the
/// CLI failed, a time limit.
pub fn branch_status(
    forge: &RepositoryForge,
    main_checkout: &Path,
    default_branch: Option<&str>,
    branch: &str,
    limit: Duration,
) -> Result<BranchStatus, String> {
    let backend = backend(forge, limit)?;
    prune::branch_status(main_checkout, backend.as_ref(), default_branch, branch)
        .map_err(|error| error.to_string().replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provider_comes_from_the_name_of_the_host() {
        let cases = [
            ("github.com", Provider::GitHub),
            ("github-work", Provider::GitHub),
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
            // A @ in the path is not a user.
            (
                "git@github.com:owner/name@v2.git",
                ("github.com", "owner/name@v2.git"),
            ),
            ("https://example.org/team/a@b", ("example.org", "team/a@b")),
        ];
        for (url, expected) in cases {
            assert_eq!(split_remote(url), Some(expected), "{url}");
        }
        assert_eq!(split_remote("/local/path"), None);
    }

    #[test]
    fn the_json_of_wt_gives_the_forge() {
        let json: Value =
            serde_json::from_str(include_str!("../tests/fixtures/wt-list.json")).unwrap();

        assert_eq!(
            from_worktrunk(&json),
            Some(RepositoryForge {
                provider: Provider::GitLab,
                host: "git.company.example".to_owned(),
                repository: Some("team/app".to_owned()),
            })
        );
        assert_eq!(from_worktrunk(&serde_json::json!({ "repo": {} })), None);
    }

    #[test]
    fn a_provider_without_backend_gives_an_error() {
        let forge = RepositoryForge {
            provider: Provider::GitLab,
            host: "gitlab.com".to_owned(),
            repository: Some("team/app".to_owned()),
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
