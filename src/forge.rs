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
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::azure::{self, AzureDevOps};
use crate::git;
use crate::gitea::Gitea;
use crate::github::GitHub;
use crate::gitlab::GitLab;
use crate::herdr::Error;
use crate::process;
use crate::sync::lock;
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

/// The number of pull requests of a branch that one call reads. A branch
/// has few: a full page gives an error.
pub const PAGE_SIZE: usize = 50;

/// Read the JSON of a CLI. `label` names the command in the error.
pub fn parse_json<T: DeserializeOwned>(text: &str, label: &str) -> Result<T, Error> {
    serde_json::from_str(text)
        .map_err(|error| Error::Command(format!("{label}: invalid JSON: {error}")))
}

/// Read one page of pull requests. A full page gives an error: it can miss
/// an open pull request, and then the worktree stays.
pub fn parse_page<T: DeserializeOwned>(text: &str, label: &str) -> Result<Vec<T>, Error> {
    let page: Vec<T> = parse_json(text, label)?;
    if page.len() >= PAGE_SIZE {
        return Err(Error::Command(format!(
            "{label}: {PAGE_SIZE} results or more for this branch, the list can miss one"
        )));
    }
    Ok(page)
}

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
    /// states, in any order. A pull request from a fork does not count.
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error>;

    /// Return true if the commit `head` contains the commit `commit`, as
    /// the forge sees them. `head` is the head of a pull request from
    /// `pull_requests` of this backend, never empty. False means "no
    /// proof": the forge does not know `commit`, or the backend cannot tell.
    fn contains(&self, head: &str, commit: &str) -> bool;

    /// The command that gets back the branch of a closed pull request.
    /// `commit` is the local commit that the pull request contains.
    fn recover_command(&self, pull_request: &PullRequest, commit: &str, branch: &str) -> String;

    /// The provider of the forge, for its words.
    fn provider(&self) -> Provider;
}

/// How long the observer keeps the forge of a repository: `wt list` reads
/// git for each worktree.
const OBSERVER_FORGE_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// The provider of a forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

    /// The number of a pull request as the forge writes it: `!3` on GitLab
    /// and Azure DevOps (`#3` is an issue or a work item there), `#3`
    /// elsewhere.
    pub fn number_text(self, number: u64) -> String {
        match self {
            Provider::GitLab | Provider::AzureDevOps => format!("!{number}"),
            _ => format!("#{number}"),
        }
    }

    /// The short name of a pull request: `MR` on GitLab, `PR` elsewhere.
    pub fn short_name(self) -> &'static str {
        match self {
            Provider::GitLab => "MR",
            _ => "PR",
        }
    }

    /// The name of a pull request: `merge request` on GitLab.
    pub fn long_name(self) -> &'static str {
        match self {
            Provider::GitLab => "merge request",
            _ => "pull request",
        }
    }

    /// The name and the number of a pull request: `pull request #3`,
    /// `merge request !3`.
    pub fn reference(self, number: u64) -> String {
        format!("{} {}", self.long_name(), self.number_text(number))
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
        let host_name = host.split(':').next().unwrap_or_default();
        if host.contains("github") {
            Provider::GitHub
        } else if host.contains("gitlab") {
            Provider::GitLab
        } else if host.contains("forgejo") || host.contains("codeberg") {
            Provider::Forgejo
        } else if host.contains("gitea") {
            Provider::Gitea
        } else if ["dev.azure.com", "ssh.dev.azure.com"].contains(&host_name)
            || host_name.ends_with(".visualstudio.com")
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
    pub repository: Option<Repository>,
}

/// The repository on the forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repository {
    /// The path of the repository: `owner/name`, or `group/subgroup/name`
    /// on GitLab.
    Path(String),
    /// A repository of Azure DevOps: its organization, its project and its
    /// name.
    Azure(azure::Location),
}

/// Why the plugin cannot ask the forge of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeError {
    /// No remote `origin`: the branches can have no pull request.
    NoRemote,
    /// The plugin cannot read the remote `origin`.
    UnreadableRemote(String),
    /// The provider has no backend.
    NoBackend(Provider),
    /// The forge has no backend for this host or this repository.
    Unsupported(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::NoRemote => write!(formatter, "no remote origin"),
            ForgeError::UnreadableRemote(url) => write!(formatter, "cannot read the remote {url}"),
            ForgeError::NoBackend(provider) => {
                write!(formatter, "no backend for {} yet", provider.name())
            }
            ForgeError::Unsupported(why) => write!(formatter, "{why}"),
        }
    }
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
        // wt gives a wrong project for some remotes of Azure DevOps:
        // `resolve` reads the remote itself.
        (Some(owner), Some(name))
            if !owner.is_empty() && !name.is_empty() && provider != Provider::AzureDevOps =>
        {
            Some(Repository::Path(format!("{owner}/{name}")))
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
pub fn from_origin(main_checkout: &Path) -> Result<RepositoryForge, ForgeError> {
    let url = git::origin_url(main_checkout).ok_or(ForgeError::NoRemote)?;
    from_remote(&url).ok_or(ForgeError::UnreadableRemote(url))
}

/// The forge of a remote URL, from the name of its host. None for a local
/// path.
fn from_remote(url: &str) -> Option<RepositoryForge> {
    let (host, path) = split_remote(url)?;
    let provider = Provider::from_host(host);
    if provider == Provider::AzureDevOps {
        return azure_forge(url).or(Some(RepositoryForge {
            provider,
            host: host.to_owned(),
            repository: None,
        }));
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let has_owner_and_name = path
        .split_once('/')
        .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty());
    Some(RepositoryForge {
        provider: Provider::from_host(host),
        host: host.to_owned(),
        repository: has_owner_and_name.then(|| Repository::Path(path.to_owned())),
    })
}

/// The forge of a repository: from the JSON of `wt list` first, with the
/// forge config of the user (`forge.platform` for a self-hosted host),
/// else from the host of `origin`. The cleaner and the observer use this
/// order.
pub fn resolve(
    main_checkout: &Path,
    listed: Option<&Value>,
) -> Result<RepositoryForge, ForgeError> {
    match listed.and_then(from_worktrunk) {
        // The remote of Azure DevOps gives its 3 parts. wt can give the
        // provider of a host with another name (Azure DevOps Server).
        Some(forge) if forge.provider == Provider::AzureDevOps => {
            let url = git::origin_url(main_checkout).ok_or(ForgeError::NoRemote)?;
            Ok(azure_forge(&url).unwrap_or(forge))
        }
        Some(forge) => Ok(forge),
        None => from_origin(main_checkout),
    }
}

/// The forge of a remote of Azure DevOps, with its organization, its
/// project and its repository. None if the path has another form.
fn azure_forge(url: &str) -> Option<RepositoryForge> {
    let (host, path) = split_remote(url)?;
    let location = azure::parse_remote_path(host, path)?;
    // The API of `ssh.dev.azure.com` is on `dev.azure.com`.
    let host = if host.eq_ignore_ascii_case("ssh.dev.azure.com") {
        "dev.azure.com"
    } else {
        host
    };
    Some(RepositoryForge {
        provider: Provider::AzureDevOps,
        host: host.to_owned(),
        repository: Some(Repository::Azure(location)),
    })
}

/// The forge of a repository, for the observer: as `resolve`, with one
/// call to wt for each repository in one hour at most.
pub fn of_repository(main_checkout: &Path) -> Result<RepositoryForge, ForgeError> {
    type Known = HashMap<PathBuf, (Instant, Result<RepositoryForge, ForgeError>)>;
    static KNOWN: OnceLock<Mutex<Known>> = OnceLock::new();
    let known = KNOWN.get_or_init(Mutex::default);
    if let Some((time, forge)) = lock(known).get(main_checkout) {
        if time.elapsed() < OBSERVER_FORGE_LIFETIME {
            return forge.clone();
        }
    }
    let forge = resolve(main_checkout, wt::list_json(main_checkout).as_ref());
    lock(known).insert(main_checkout.to_owned(), (Instant::now(), forge.clone()));
    forge
}

/// The host behind an ssh alias (`Host github-work` in `~/.ssh/config`),
/// from `ssh -G`. The host itself if ssh does not know it. The process
/// keeps each answer: the config of ssh rarely changes.
fn real_host(host: &str) -> String {
    static KNOWN: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let known = KNOWN.get_or_init(Mutex::default);
    if let Some(real) = lock(known).get(host) {
        return real.clone();
    }
    let real = host_from_ssh(host);
    lock(known).insert(host.to_owned(), real.clone());
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
pub fn api_host(forge: &RepositoryForge, public_host: &str) -> Result<String, ForgeError> {
    if forge.host.is_empty() {
        return Err(ForgeError::Unsupported(format!(
            "no host for {}",
            forge.provider.name()
        )));
    }
    Ok(if forge.host == public_host {
        forge.host.clone()
    } else {
        real_host(&forge.host)
    })
}

/// The path of the repository of `forge`: `owner/name`, or
/// `group/subgroup/name`.
pub fn repository_path(forge: &RepositoryForge) -> Result<&str, ForgeError> {
    match &forge.repository {
        Some(Repository::Path(path)) => Ok(path),
        _ => Err(ForgeError::Unsupported(format!(
            "no path for the repository on {}",
            forge.provider.name()
        ))),
    }
}

/// The path of the repository of `forge` as `owner/name`: GitHub, Gitea and
/// Forgejo have no subgroups.
pub fn owner_and_name(forge: &RepositoryForge) -> Result<&str, ForgeError> {
    let path = repository_path(forge)?;
    if path.matches('/').count() != 1 {
        return Err(ForgeError::Unsupported(format!(
            "no owner and name for the repository on {}: {path}",
            forge.provider.name()
        )));
    }
    Ok(path)
}

/// The backend of the provider of `forge`. An error tells why the plugin
/// cannot ask the forge: no backend, no repository.
pub fn backend(forge: &RepositoryForge, limit: Duration) -> Result<Box<dyn Backend>, ForgeError> {
    Ok(match forge.provider {
        Provider::GitHub => Box::new(GitHub::from_forge(forge, limit)?),
        Provider::GitLab => Box::new(GitLab::from_forge(forge, limit)?),
        Provider::Gitea | Provider::Forgejo => Box::new(Gitea::from_forge(forge, limit)?),
        Provider::AzureDevOps => Box::new(AzureDevOps::from_forge(forge, limit)?),
        Provider::Unknown => return Err(ForgeError::NoBackend(forge.provider)),
    })
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
    use crate::test_support::{git_in, test_folder};

    #[test]
    fn a_full_page_of_pull_requests_gives_an_error() {
        let page = |count: u64| {
            let numbers: Vec<String> = (1..=count).map(|number| number.to_string()).collect();
            format!("[{}]", numbers.join(","))
        };

        assert_eq!(parse_page::<u64>(&page(49), "x").unwrap().len(), 49);
        assert_eq!(
            parse_page::<u64>(&page(50), "gh pr list")
                .unwrap_err()
                .to_string(),
            "gh pr list: 50 results or more for this branch, the list can miss one"
        );
        assert!(parse_page::<u64>("{\"message\":\"x\"}", "x").is_err());
    }

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
                    repository: Some(Repository::Path("owpac/herdr-worktrunk".to_owned())),
                }),
                "{url}"
            );
        }
        // A numeric owner is not a port.
        assert_eq!(
            from_remote("git@github.com:1234/repo.git").and_then(|forge| forge.repository),
            Some(Repository::Path("1234/repo".to_owned()))
        );
    }

    #[test]
    fn a_gitlab_url_keeps_its_subgroups() {
        assert_eq!(
            from_remote("git@gitlab.com:group/sub/app.git"),
            Some(RepositoryForge {
                provider: Provider::GitLab,
                host: "gitlab.com".to_owned(),
                repository: Some(Repository::Path("group/sub/app".to_owned())),
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
                repository: Some(Repository::Path("forgejo/forgejo".to_owned())),
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
                repository: Some(Repository::Path("team/app".to_owned())),
            })
        );
        assert_eq!(from_worktrunk(&serde_json::json!({ "repo": {} })), None);
    }

    #[test]
    fn a_provider_without_backend_gives_an_error() {
        let forge = RepositoryForge {
            provider: Provider::Unknown,
            host: "git.company.example".to_owned(),
            repository: Some(Repository::Path("team/app".to_owned())),
        };

        assert_eq!(
            backend(&forge, Duration::from_secs(1))
                .err()
                .map(|error| error.to_string()),
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
        let expected = RepositoryForge {
            provider: Provider::AzureDevOps,
            host: "dev.azure.com".to_owned(),
            repository: Some(Repository::Azure(azure::Location {
                organization: "myorg".to_owned(),
                project: "My Project".to_owned(),
                name: "my repo".to_owned(),
            })),
        };

        assert_eq!(
            from_remote("git@ssh.dev.azure.com:v3/myorg/My%20Project/my%20repo").as_ref(),
            Some(&expected)
        );
        assert_eq!(
            from_remote("https://myorg@dev.azure.com/myorg/My%20Project/_git/my%20repo").as_ref(),
            Some(&expected)
        );
        let limit = Duration::from_secs(1);
        assert!(backend(&expected, limit).is_ok());
        let without_project = RepositoryForge {
            repository: Some(Repository::Path("myorg/my-repo".to_owned())),
            ..expected
        };
        assert_eq!(
            backend(&without_project, limit)
                .err()
                .map(|error| error.to_string()),
            Some("no organization, project and name on Azure DevOps: myorg/my-repo".to_owned())
        );
    }

    #[test]
    fn the_remote_gives_the_parts_of_azure_devops_not_wt() {
        // wt 0.81 gives a wrong project for a remote with
        // DefaultCollection, and keeps the name encoded.
        let folder = test_folder("azure-remote");
        git_in(&folder, &["init", "-q"]);
        git_in(
            &folder,
            &[
                "remote",
                "add",
                "origin",
                "https://myorg.visualstudio.com/DefaultCollection/My%20Project/_git/my%20repo",
            ],
        );
        let json = serde_json::json!({ "repo": { "forge": {
            "provider": "azure-devops",
            "host": "myorg.visualstudio.com",
            "owner": "myorg",
            "name": "my%20repo",
            "project": "DefaultCollection",
            "remote": "origin"
        } } });

        assert_eq!(
            from_worktrunk(&json).and_then(|forge| forge.repository),
            None
        );
        assert_eq!(
            resolve(&folder, Some(&json)),
            Ok(RepositoryForge {
                provider: Provider::AzureDevOps,
                host: "myorg.visualstudio.com".to_owned(),
                repository: Some(Repository::Azure(azure::Location {
                    organization: "myorg".to_owned(),
                    project: "My Project".to_owned(),
                    name: "my repo".to_owned(),
                })),
            })
        );
    }

    #[test]
    fn an_azure_host_with_a_port_is_azure_devops() {
        assert_eq!(
            Provider::from_host("dev.azure.com:443"),
            Provider::AzureDevOps
        );
    }

    #[test]
    fn each_provider_gets_its_backend() {
        let forge = |provider, host: &str| RepositoryForge {
            provider,
            host: host.to_owned(),
            repository: Some(Repository::Path("group/sub/app".to_owned())),
        };
        let limit = Duration::from_secs(1);

        let github = RepositoryForge {
            repository: Some(Repository::Path("owner/name".to_owned())),
            ..forge(Provider::GitHub, "github.com")
        };
        assert!(backend(&github, limit).is_ok());
        assert!(backend(&forge(Provider::GitLab, "gitlab.com"), limit).is_ok());
        let codeberg = RepositoryForge {
            repository: Some(Repository::Path("forgejo/forgejo".to_owned())),
            ..forge(Provider::Forgejo, "codeberg.org")
        };
        assert!(backend(&codeberg, limit).is_ok());
        // Gitea has no subgroups.
        assert_eq!(
            backend(&forge(Provider::Gitea, "gitea.com"), limit)
                .err()
                .map(|error| error.to_string()),
            Some("no owner and name for the repository on Gitea: group/sub/app".to_owned())
        );
        assert_eq!(
            backend(&forge(Provider::GitLab, ""), limit)
                .err()
                .map(|error| error.to_string()),
            Some("no host for GitLab".to_owned())
        );
        // GitHub has no subgroups.
        assert_eq!(
            backend(&forge(Provider::GitHub, "github.com"), limit)
                .err()
                .map(|error| error.to_string()),
            Some("no owner and name for the repository on GitHub: group/sub/app".to_owned())
        );
        let without_path = RepositoryForge {
            repository: None,
            ..forge(Provider::GitLab, "gitlab.com")
        };
        assert_eq!(
            backend(&without_path, limit)
                .err()
                .map(|error| error.to_string()),
            Some("no path for the repository on GitLab".to_owned())
        );
    }
}
