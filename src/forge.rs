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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::azure::{self, AzureDevOps};
use crate::git;
use crate::gitea::{Flavor, Gitea};
use crate::github::GitHub;
use crate::gitlab::GitLab;
use crate::herdr::Error;
use crate::process;
use crate::wt;

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

/// The calls to one forge, for one repository. The threads of the cleaner
/// share one backend for each repository.
pub trait Backend: Send + Sync {
    /// The pull requests of `branch` in the repository itself, in all the
    /// states, the newest first. A pull request from a fork does not count.
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error>;

    /// Return true if the commit `head` contains the commit `commit`, as
    /// the forge sees them. False if the forge does not know `commit`.
    fn contains(&self, head: &str, commit: &str) -> bool;

    /// The command that gets back the branch of a closed pull request.
    /// `commit` is the local commit that the pull request contains.
    fn recover_command(&self, pull_request: &PullRequest, commit: &str, branch: &str) -> String;

    /// The name and the number of a pull request, as the forge writes them:
    /// `pull request #3`, or `merge request !3` on GitLab.
    fn reference(&self, number: u64) -> String;
}

/// How long the observer keeps the forge of a repository: `wt list` reads
/// git for each worktree.
const OBSERVER_FORGE_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// The provider of a forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    GitLab,
    Gitea,
    /// Forgejo, and Codeberg, its public host.
    Forgejo,
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
            Provider::Forgejo => "Forgejo",
            Provider::AzureDevOps => "Azure DevOps",
            Provider::Unknown => "an unknown forge",
        }
    }

    /// The provider of a `provider` value of worktrunk.
    fn from_worktrunk(provider: &str) -> Provider {
        match provider {
            "github" => Provider::GitHub,
            "gitlab" => Provider::GitLab,
            "gitea" => Provider::Gitea,
            "forgejo" => Provider::Forgejo,
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
        } else if host.contains("forgejo") || host.contains("codeberg") {
            Provider::Forgejo
        } else if host.contains("gitea") {
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
        // `[user@]host[:port]/owner/name`. The port of ssh is not the port
        // of the web: only an URL of the web keeps its port.
        Some(rest) => {
            let (host_and_port, path) = without_user(rest).split_once('/')?;
            let is_web = url.starts_with("https://") || url.starts_with("http://");
            let host = if is_web {
                host_and_port
            } else {
                host_and_port.split(':').next().unwrap_or_default()
            };
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
    /// The repository on the forge: `owner/name`. On Azure DevOps:
    /// `organization/project/repository`, decoded (a project name can have
    /// spaces, but no `/`).
    pub repository: Option<String>,
}

/// The forge that `wt list --format json` gives in `repo.forge`.
pub fn from_worktrunk(json: &Value) -> Option<RepositoryForge> {
    let forge = &json["repo"]["forge"];
    let host = forge["host"].as_str().unwrap_or_default().to_owned();
    // wt does not know some hosts, for example codeberg.org: their name
    // then gives the provider.
    let provider = match Provider::from_worktrunk(forge["provider"].as_str()?) {
        Provider::Unknown => Provider::from_host(&host),
        provider => provider,
    };
    let repository = match (forge["owner"].as_str(), forge["name"].as_str()) {
        (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() => {
            match (provider, forge["project"].as_str()) {
                // wt gives the project of Azure DevOps apart, encoded.
                (Provider::AzureDevOps, Some(project)) => {
                    Some(format!("{owner}/{}/{name}", azure::percent_decode(project)))
                }
                (Provider::AzureDevOps, None) => None,
                _ => Some(format!("{owner}/{name}")),
            }
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
    from_remote(&url).ok_or_else(|| format!("cannot read the remote {url}"))
}

/// The forge of a remote URL, from the name of its host. None for a local
/// path.
fn from_remote(url: &str) -> Option<RepositoryForge> {
    let (host, path) = split_remote(url)?;
    let provider = Provider::from_host(host);
    if provider == Provider::AzureDevOps {
        let repository =
            azure::parse_remote_path(host, path).map(|(organization, project, repository)| {
                format!("{organization}/{project}/{repository}")
            });
        // The API of `ssh.dev.azure.com` is on `dev.azure.com`.
        let host = if host.eq_ignore_ascii_case("ssh.dev.azure.com") {
            "dev.azure.com"
        } else {
            host
        };
        return Some(RepositoryForge {
            provider,
            host: host.to_owned(),
            repository,
        });
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let has_owner_and_name = path
        .split_once('/')
        .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty());
    Some(RepositoryForge {
        provider: Provider::from_host(host),
        host: host.to_owned(),
        repository: has_owner_and_name.then(|| path.to_owned()),
    })
}

/// The error of a repository without a remote `origin`: its branches can
/// have no pull request.
pub const NO_REMOTE: &str = "no remote origin";

/// The forge of a repository: from the JSON of `wt list` first, with the
/// forge config of the user (`forge.platform` for a self-hosted host),
/// else from the host of `origin`. The cleaner and the observer use this
/// order.
pub fn resolve(main_checkout: &Path, listed: Option<&Value>) -> Result<RepositoryForge, String> {
    match listed.and_then(from_worktrunk) {
        Some(forge) => Ok(forge),
        None => from_origin(main_checkout),
    }
}

/// The forge of a repository, for the observer: as `resolve`, with one
/// call to wt for each repository in one hour at most.
pub fn of_repository(main_checkout: &Path) -> Result<RepositoryForge, String> {
    type Known = HashMap<PathBuf, (Instant, Result<RepositoryForge, String>)>;
    static KNOWN: OnceLock<Mutex<Known>> = OnceLock::new();
    let known = KNOWN.get_or_init(Mutex::default);
    if let Some((time, forge)) = known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(main_checkout)
    {
        if time.elapsed() < OBSERVER_FORGE_LIFETIME {
            return forge.clone();
        }
    }
    let forge = resolve(main_checkout, wt::list_json(main_checkout).as_ref());
    known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(main_checkout.to_owned(), (Instant::now(), forge.clone()));
    forge
}

/// The host behind an ssh alias (`Host github-work` in `~/.ssh/config`),
/// from `ssh -G`. The host itself if ssh does not know it. The process
/// keeps each answer: the config of ssh rarely changes.
fn real_host(host: &str) -> String {
    static KNOWN: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let known = KNOWN.get_or_init(Mutex::default);
    if let Some(real) = known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(host)
    {
        return real.clone();
    }
    let real = host_from_ssh(host);
    known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(host.to_owned(), real.clone());
    real
}

/// The `hostname` that `ssh -G` gives for `host`.
fn host_from_ssh(host: &str) -> String {
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

/// The host of the API of a forge: the host of the remote, or the host
/// behind its ssh alias. The public host of the provider needs no call to
/// ssh.
fn api_host(forge: &RepositoryForge, public_host: &str) -> Result<String, String> {
    if forge.host.is_empty() {
        return Err(format!("no host for {}", forge.provider.name()));
    }
    Ok(if forge.host == public_host {
        forge.host.clone()
    } else {
        real_host(&forge.host)
    })
}

/// The backend of the provider of `forge`. An error tells why the plugin
/// cannot ask the forge: no backend, no repository.
pub fn backend(forge: &RepositoryForge, limit: Duration) -> Result<Box<dyn Backend>, String> {
    let provider = forge.provider.name();
    let repository = || {
        forge
            .repository
            .clone()
            .ok_or_else(|| format!("no path for the repository on {provider}"))
    };
    match forge.provider {
        Provider::GitHub => {
            let host = api_host(forge, "github.com")?;
            if host != "github.com" {
                return Err(format!("GitHub on {host} has no backend yet"));
            }
            let repository = repository()?;
            if repository.matches('/').count() != 1 {
                return Err(format!(
                    "no owner and name for the repository on GitHub: {repository}"
                ));
            }
            Ok(Box::new(GitHub::new(repository, limit)))
        }
        Provider::GitLab => Ok(Box::new(GitLab::new(
            api_host(forge, "gitlab.com")?,
            repository()?,
            limit,
        ))),
        Provider::AzureDevOps => {
            let repository = repository()?;
            let parts: Vec<&str> = repository.split('/').collect();
            let [organization, project, name] = parts.as_slice() else {
                return Err(format!(
                    "no organization, project and name on Azure DevOps: {repository}"
                ));
            };
            Ok(Box::new(AzureDevOps::new(
                azure::organization_url(&forge.host, organization),
                (*project).to_owned(),
                (*name).to_owned(),
                limit,
            )))
        }
        Provider::Gitea | Provider::Forgejo => {
            let (flavor, public_host) = if forge.provider == Provider::Gitea {
                (Flavor::Gitea, "gitea.com")
            } else {
                (Flavor::Forgejo, "codeberg.org")
            };
            let repository = repository()?;
            if repository.matches('/').count() != 1 {
                return Err(format!(
                    "no owner and name for the repository on {provider}: {repository}"
                ));
            }
            Ok(Box::new(Gitea::new(
                api_host(forge, public_host)?,
                repository,
                flavor,
                limit,
            )))
        }
        _ => Err(format!("no backend for {provider} yet")),
    }
}

/// Encode a value for a path segment or a query of a URL: each byte
/// outside the unreserved characters of RFC 3986 becomes `%XX`. The braces
/// get encoded too: tea replaces `{owner}`, `{repo}` and `{branch}`.
pub fn encode_url_part(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
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
            ("codeberg.org", Provider::Forgejo),
            ("forgejo.example.org", Provider::Forgejo),
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
            // The web keeps its port, ssh does not.
            (
                "https://gitlab.example.com:8443/group/app.git",
                ("gitlab.example.com:8443", "group/app.git"),
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(split_remote(url), Some(expected), "{url}");
        }
        assert_eq!(split_remote("/local/path"), None);
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
                from_remote(url),
                Some(RepositoryForge {
                    provider: Provider::GitHub,
                    host: "github.com".to_owned(),
                    repository: Some("owpac/herdr-worktrunk".to_owned()),
                }),
                "{url}"
            );
        }
        // A numeric owner is not a port.
        assert_eq!(
            from_remote("git@github.com:1234/repo.git").and_then(|forge| forge.repository),
            Some("1234/repo".to_owned())
        );
    }

    #[test]
    fn a_gitlab_url_keeps_its_subgroups() {
        assert_eq!(
            from_remote("git@gitlab.com:group/sub/app.git"),
            Some(RepositoryForge {
                provider: Provider::GitLab,
                host: "gitlab.com".to_owned(),
                repository: Some("group/sub/app".to_owned()),
            })
        );
    }

    #[test]
    fn a_remote_without_owner_or_name_has_no_repository() {
        assert_eq!(
            from_remote("https://github.com/owpac").and_then(|forge| forge.repository),
            None
        );
        assert_eq!(from_remote("/work/bare-repo.git"), None);
    }

    #[test]
    fn a_host_that_wt_does_not_know_gets_the_provider_of_its_name() {
        // The answer of wt for a remote on codeberg.org.
        let json = serde_json::json!({ "repo": { "forge": {
            "url": "https://codeberg.org/forgejo/forgejo",
            "provider": "unknown",
            "host": "codeberg.org",
            "owner": "forgejo",
            "name": "forgejo",
            "remote": "origin"
        } } });

        assert_eq!(
            from_worktrunk(&json),
            Some(RepositoryForge {
                provider: Provider::Forgejo,
                host: "codeberg.org".to_owned(),
                repository: Some("forgejo/forgejo".to_owned()),
            })
        );
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
            provider: Provider::Unknown,
            host: "git.company.example".to_owned(),
            repository: Some("team/app".to_owned()),
        };

        assert_eq!(
            backend(&forge, Duration::from_secs(1)).err(),
            Some("no backend for an unknown forge yet".to_owned())
        );
    }

    #[test]
    fn encode_url_part_keeps_only_the_unreserved_characters() {
        assert_eq!(encode_url_part("group/sub/app"), "group%2Fsub%2Fapp");
        assert_eq!(encode_url_part("feat/forge-layers"), "feat%2Fforge-layers");
        assert_eq!(encode_url_part("v1.2_x~y"), "v1.2_x~y");
        assert_eq!(encode_url_part("a+b c&d=e#f"), "a%2Bb%20c%26d%3De%23f");
        assert_eq!(encode_url_part("é"), "%C3%A9");
        assert_eq!(encode_url_part("feat/{branch}"), "feat%2F%7Bbranch%7D");
    }

    #[test]
    fn azure_devops_keeps_its_project() {
        // The answer of wt for a remote on dev.azure.com.
        let json = serde_json::json!({ "repo": { "forge": {
            "url": "https://dev.azure.com/myorg/My%20Project/_git/my-repo",
            "provider": "azure-devops",
            "host": "dev.azure.com",
            "owner": "myorg",
            "name": "my-repo",
            "project": "My%20Project",
            "remote": "origin"
        } } });
        let expected = RepositoryForge {
            provider: Provider::AzureDevOps,
            host: "dev.azure.com".to_owned(),
            repository: Some("myorg/My Project/my-repo".to_owned()),
        };

        assert_eq!(from_worktrunk(&json).as_ref(), Some(&expected));
        assert_eq!(
            from_remote("git@ssh.dev.azure.com:v3/myorg/My%20Project/my-repo").as_ref(),
            Some(&expected)
        );
        assert_eq!(
            from_remote("https://myorg@dev.azure.com/myorg/My%20Project/_git/my-repo").as_ref(),
            Some(&expected)
        );
        let limit = Duration::from_secs(1);
        assert!(backend(&expected, limit).is_ok());
        let without_project = RepositoryForge {
            repository: Some("myorg/my-repo".to_owned()),
            ..expected
        };
        assert_eq!(
            backend(&without_project, limit).err(),
            Some("no organization, project and name on Azure DevOps: myorg/my-repo".to_owned())
        );
    }

    #[test]
    fn github_and_gitlab_have_a_backend() {
        let forge = |provider, host: &str| RepositoryForge {
            provider,
            host: host.to_owned(),
            repository: Some("group/sub/app".to_owned()),
        };
        let limit = Duration::from_secs(1);

        let github = RepositoryForge {
            repository: Some("owner/name".to_owned()),
            ..forge(Provider::GitHub, "github.com")
        };
        assert!(backend(&github, limit).is_ok());
        assert!(backend(&forge(Provider::GitLab, "gitlab.com"), limit).is_ok());
        let codeberg = RepositoryForge {
            repository: Some("forgejo/forgejo".to_owned()),
            ..forge(Provider::Forgejo, "codeberg.org")
        };
        assert!(backend(&codeberg, limit).is_ok());
        // Gitea has no subgroups.
        assert_eq!(
            backend(&forge(Provider::Gitea, "gitea.com"), limit).err(),
            Some("no owner and name for the repository on Gitea: group/sub/app".to_owned())
        );
        assert_eq!(
            backend(&forge(Provider::GitLab, ""), limit).err(),
            Some("no host for GitLab".to_owned())
        );
        // GitHub has no subgroups.
        assert_eq!(
            backend(&forge(Provider::GitHub, "github.com"), limit).err(),
            Some("no owner and name for the repository on GitHub: group/sub/app".to_owned())
        );
        let without_path = RepositoryForge {
            repository: None,
            ..forge(Provider::GitLab, "gitlab.com")
        };
        assert_eq!(
            backend(&without_path, limit).err(),
            Some("no path for the repository on GitLab".to_owned())
        );
    }
}
