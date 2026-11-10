//! The calls to Gitea and Forgejo, with the tea CLI: `tea api` on the REST
//! API v1.
//!
//! tea keeps a login for each host (`tea logins add`). The backend always
//! names the login of the host of the repository (`--login`): without it,
//! tea falls back to the default login, on another host.
//!
//! The backend reads the HTTP status on the standard error (`tea api -i`):
//! tea 0.16 ends with the code 0 also after an HTTP error.
//!
//! The pull requests of a branch:
//!
//! - Forgejo filters the list by its head branch (`head`).
//! - Gitea ignores this filter. The backend reads the open pull requests of
//!   the repository once, then the pull request of the branch into the
//!   default branch (`pulls/{base}/{head}`). Gitea gives one pull request
//!   for this pair, not always the last one.
//!
//! The head of a finished pull request: Gitea and Forgejo give the commit
//! of the branch now (`head.sha`), not the last commit of the pull request.
//! Thus the backend reads the commits of each finished pull request
//! (`pulls/{index}/commits`, the newest first). A pull request contains the
//! local commit if this list contains it.
//!
//! A list can have several pages. A server can give fewer items than the
//! backend asks for: the total of the list (`X-Total-Count`) tells when the
//! list is complete.

use std::collections::HashMap;
use std::env;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::forge::{
    self, Backend, ForgeError, Provider, PullRequest, PullRequestState, RepositoryForge,
};
use crate::herdr::Error;
use crate::process;
use crate::sync::lock;
use crate::url::encode_part as encode;

/// The number of items that one call asks for.
const PAGE_SIZE: usize = 50;
/// The pages of a list that the backend reads at most.
const MAX_PAGES: usize = 5;
/// How long the process keeps the list of the logins of tea.
const LOGINS_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// The API of the forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Gitea,
    Forgejo,
}

/// The backend of a repository on a Gitea or Forgejo host.
pub struct Gitea {
    /// The host of the API, with its port: `codeberg.org`.
    host: String,
    /// The repository: `owner/name`.
    repository: String,
    flavor: Flavor,
    /// The time limit of each call to tea.
    limit: Duration,
    /// The default branch of the repository, from the forge.
    default_branch: Mutex<Option<String>>,
    /// The open pull requests of the repository, on Gitea: one read for
    /// all the branches.
    open: Mutex<Option<Vec<Listed>>>,
    /// The commits of each finished pull request, by its last commit.
    commits: Mutex<HashMap<String, Vec<String>>>,
}

/// An answer of the API.
struct Response {
    /// The body, or None for the status 404.
    body: Option<String>,
    /// The number of items of the whole list (`X-Total-Count`).
    total: Option<usize>,
}

impl Gitea {
    /// The backend of `forge`: Gitea or Forgejo, with an owner and a name.
    pub fn from_forge(forge: &RepositoryForge, limit: Duration) -> Result<Gitea, ForgeError> {
        let (flavor, public_host) = if forge.provider == Provider::Gitea {
            (Flavor::Gitea, "gitea.com")
        } else {
            (Flavor::Forgejo, "codeberg.org")
        };
        Ok(Gitea::new(
            forge::api_host(forge, public_host)?,
            forge::owner_and_name(forge)?.to_owned(),
            flavor,
            limit,
        ))
    }

    pub fn new(host: String, repository: String, flavor: Flavor, limit: Duration) -> Gitea {
        Gitea {
            host,
            repository,
            flavor,
            limit,
            default_branch: Mutex::default(),
            open: Mutex::default(),
            commits: Mutex::default(),
        }
    }

    /// Run `tea api` on `endpoint` with the login of the host.
    fn api(&self, endpoint: &str) -> Result<Response, Error> {
        let login = login_of_host(&self.host, self.limit)?;
        let mut command = Command::new("tea");
        command
            .args(["api", "--login", &login, "-i", endpoint])
            .env("NO_COLOR", "1")
            // Not in a repository: tea must not read the remotes of a
            // checkout.
            .current_dir(env::temp_dir());
        let output = process::cli_output(&mut command, "tea api", self.limit)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let body = String::from_utf8_lossy(&output.stdout).into_owned();
        // The status line decides, also if tea ends with an error code.
        match status_code(&stderr) {
            Some(200..=299) => Ok(Response {
                body: Some(body),
                total: total_count(&stderr),
            }),
            Some(404) => Ok(Response {
                body: None,
                total: None,
            }),
            Some(code) => Err(Error::Command(format!(
                "tea api: HTTP {code}: {}",
                error_message(&body)
            ))),
            None => Err(Error::Command(format!(
                "tea api: {}",
                stderr
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("no HTTP status")
            ))),
        }
    }

    /// The body of `endpoint`. A 404 is an error.
    fn found(&self, endpoint: &str) -> Result<(String, Option<usize>), Error> {
        let response = self.api(endpoint)?;
        let body = response
            .body
            .ok_or_else(|| Error::Command(format!("tea api: not found: {endpoint}")))?;
        Ok((body, response.total))
    }

    /// Read the pages of a list, `MAX_PAGES` at most. Return the items, and
    /// true if the list is complete.
    fn read_list<T>(
        &self,
        endpoint: &str,
        parse: fn(&str) -> Result<Vec<T>, Error>,
    ) -> Result<(Vec<T>, bool), Error> {
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let (body, total) = self.found(&format!(
                "{endpoint}{separator}limit={PAGE_SIZE}&page={page}"
            ))?;
            let listed = parse(&body)?;
            let is_empty = listed.is_empty();
            items.extend(listed);
            if is_empty || total.is_some_and(|total| items.len() >= total) {
                return Ok((items, true));
            }
        }
        Ok((items, false))
    }

    fn repository_endpoint(&self) -> String {
        format!("/repos/{}", self.repository)
    }

    /// The default branch of the repository. The backend reads it once.
    fn default_branch(&self) -> Result<String, Error> {
        let mut known = lock(&self.default_branch);
        if let Some(branch) = known.as_ref() {
            return Ok(branch.clone());
        }
        let (body, _) = self.found(&self.repository_endpoint())?;
        let branch = parse_default_branch(&body)?;
        *known = Some(branch.clone());
        Ok(branch)
    }

    /// The pull requests of `branch` on Forgejo: the list with the filter
    /// of the head branch.
    fn forgejo_pull_requests(&self, branch: &str) -> Result<Vec<Listed>, Error> {
        let endpoint = format!(
            "{}/pulls?state=all&head={}",
            self.repository_endpoint(),
            encode(branch)
        );
        let (listed, is_complete) = self.read_list(&endpoint, parse_pulls)?;
        if !is_complete {
            return Err(Error::Command(format!(
                "more than {} pull requests for this branch",
                listed.len()
            )));
        }
        // An old version of Forgejo ignores the filter: the list then has
        // the pull requests of other branches.
        if listed.iter().any(|pull| pull.branch() != branch) {
            return Err(Error::Command(
                "the forge ignores the filter of the head branch".to_owned(),
            ));
        }
        Ok(listed
            .into_iter()
            .filter(Listed::is_of_repository)
            .collect())
    }

    /// The open pull requests of the repository, for all the branches. The
    /// backend reads them once. The other threads wait for this read.
    fn open_pull_requests(&self) -> Result<Vec<Listed>, Error> {
        let mut known = lock(&self.open);
        if let Some(open) = known.as_ref() {
            return Ok(open.clone());
        }
        let endpoint = format!("{}/pulls?state=open", self.repository_endpoint());
        let (open, is_complete) = self.read_list(&endpoint, parse_pulls)?;
        // A missed open pull request can give a false proof of the end.
        if !is_complete {
            return Err(Error::Command(format!(
                "more than {} open pull requests",
                open.len()
            )));
        }
        *known = Some(open.clone());
        Ok(open)
    }

    /// The pull requests of `branch` on Gitea: its open pull requests, or
    /// else one pull request into the default branch.
    fn gitea_pull_requests(&self, branch: &str) -> Result<Vec<Listed>, Error> {
        let open: Vec<Listed> = self
            .open_pull_requests()?
            .into_iter()
            .filter(|pull| pull.branch() == branch && pull.is_of_repository())
            .collect();
        // An open pull request decides alone.
        if !open.is_empty() {
            return Ok(open);
        }
        let endpoint = format!(
            "{}/pulls/{}/{}",
            self.repository_endpoint(),
            encode(&self.default_branch()?),
            encode(branch)
        );
        Ok(match self.api(&endpoint)?.body {
            Some(body) => Some(parse_pull(&body)?)
                .filter(Listed::is_of_repository)
                .into_iter()
                .collect(),
            None => Vec::new(),
        })
    }

    /// The commits of a pull request, the newest first. A list that is not
    /// complete keeps its newest commits: the proof can then miss an old
    /// commit, and the worktree stays.
    fn pull_request_commits(&self, number: u64) -> Result<Vec<String>, Error> {
        let endpoint = format!(
            "{}/pulls/{number}/commits?files=false&verification=false",
            self.repository_endpoint()
        );
        Ok(self.read_list(&endpoint, parse_commits)?.0)
    }
}

impl Backend for Gitea {
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error> {
        let listed = match self.flavor {
            Flavor::Forgejo => self.forgejo_pull_requests(branch)?,
            Flavor::Gitea => self.gitea_pull_requests(branch)?,
        };
        // An open pull request decides alone: the commits of the finished
        // ones are not necessary.
        let has_open = listed
            .iter()
            .any(|pull| pull.state() == PullRequestState::Open);
        let mut pull_requests = Vec::new();
        for pull in listed {
            let state = pull.state();
            let head = if has_open || state == PullRequestState::Open {
                pull.head.sha.unwrap_or_default()
            } else {
                // The commit of the branch now is not the last commit of a
                // finished pull request.
                let commits = self.pull_request_commits(pull.number)?;
                let head = commits.first().cloned().unwrap_or_default();
                lock(&self.commits).insert(head.clone(), commits);
                head
            };
            pull_requests.push(PullRequest {
                number: pull.number,
                state,
                head,
            });
        }
        Ok(pull_requests)
    }

    /// The commits of the finished pull requests come from
    /// `pull_requests`: no new call.
    fn contains(&self, head: &str, commit: &str) -> bool {
        lock(&self.commits)
            .get(head)
            .is_some_and(|commits| commits.iter().any(|known| known == commit))
    }

    /// Gitea and Forgejo keep the commits of a pull request in
    /// `refs/pull/<number>/head`.
    fn recover_command(&self, pull_request: &PullRequest, _commit: &str, branch: &str) -> String {
        format!(
            "git fetch origin pull/{}/head:{branch}",
            pull_request.number
        )
    }

    fn provider(&self) -> Provider {
        match self.flavor {
            Flavor::Gitea => Provider::Gitea,
            Flavor::Forgejo => Provider::Forgejo,
        }
    }
}

/// The side of a pull request: its branch, its commit, its repository.
#[derive(Clone, Debug, Default, Deserialize)]
struct Side {
    #[serde(default)]
    label: String,
    #[serde(rename = "ref", default)]
    reference: String,
    sha: Option<String>,
    /// -1 if the forge does not know the repository any more: a deleted
    /// fork, or a pull request of the AGit flow.
    repo_id: Option<i64>,
}

/// A pull request in the JSON of Gitea and Forgejo.
#[derive(Clone, Debug, Deserialize)]
struct Listed {
    number: u64,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    head: Side,
    #[serde(default)]
    base: Side,
}

impl Listed {
    /// The head branch. After the deletion of the branch, `ref` becomes
    /// `refs/pull/<number>/head`, and `label` keeps the name of the branch:
    /// `owner:branch` for a fork. A branch name cannot contain `:`.
    fn branch(&self) -> &str {
        if self.head.reference.starts_with("refs/pull/") {
            self.head
                .label
                .rsplit_once(':')
                .map_or(self.head.label.as_str(), |(_, branch)| branch)
        } else {
            &self.head.reference
        }
    }

    /// Return true if the head branch is in the repository itself, not in
    /// a fork.
    fn is_of_repository(&self) -> bool {
        self.head.repo_id.is_some_and(|id| id > 0) && self.head.repo_id == self.base.repo_id
    }

    fn state(&self) -> PullRequestState {
        match (self.state.as_str(), self.merged) {
            (_, true) => PullRequestState::Merged,
            ("closed", false) => PullRequestState::Closed,
            // "open", and the states that the plugin does not know.
            _ => PullRequestState::Open,
        }
    }
}

fn parse_pulls(text: &str) -> Result<Vec<Listed>, Error> {
    forge::parse_json(text, "tea api")
}

fn parse_pull(text: &str) -> Result<Listed, Error> {
    forge::parse_json(text, "tea api")
}

/// The commits of a pull request, in the order of the JSON.
fn parse_commits(text: &str) -> Result<Vec<String>, Error> {
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    let commits: Vec<Commit> = forge::parse_json(text, "tea api")?;
    Ok(commits.into_iter().map(|commit| commit.sha).collect())
}

fn parse_default_branch(text: &str) -> Result<String, Error> {
    #[derive(Deserialize)]
    struct Repository {
        default_branch: String,
    }
    let repository: Repository = forge::parse_json(text, "tea api")?;
    Ok(repository.default_branch)
}

/// The status code of the line `HTTP/1.1 404 Not Found` that `tea api -i`
/// writes on the standard error.
fn status_code(stderr: &str) -> Option<u16> {
    stderr
        .lines()
        .find(|line| line.starts_with("HTTP/"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The header `X-Total-Count` that `tea api -i` writes on the standard
/// error: the number of items of the whole list.
fn total_count(stderr: &str) -> Option<usize> {
    stderr.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("x-total-count")
            .then(|| value.trim().parse().ok())?
    })
}

/// The message of an error of the API, or the body itself.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().chars().take(200).collect())
}

/// The name of the login of tea for `host`, from `tea logins list`. The
/// process keeps the list for 10 minutes.
fn login_of_host(host: &str, limit: Duration) -> Result<String, Error> {
    type Logins = Option<(Instant, Vec<Login>)>;
    static LOGINS: OnceLock<Mutex<Logins>> = OnceLock::new();
    let mut known = lock(LOGINS.get_or_init(Mutex::default));
    let is_fresh = known
        .as_ref()
        .is_some_and(|(time, _)| time.elapsed() < LOGINS_LIFETIME);
    if !is_fresh {
        let mut command = Command::new("tea");
        command
            .args(["logins", "list", "--output", "json"])
            .env("NO_COLOR", "1")
            .current_dir(env::temp_dir());
        let text = process::run_cli(&mut command, "tea logins list", limit)?;
        *known = Some((Instant::now(), parse_logins(&text)?));
    }
    let logins = known
        .as_ref()
        .map(|(_, logins)| logins.as_slice())
        .unwrap_or_default();
    login_for(logins, host)
        .ok_or_else(|| Error::Command(format!("no tea login for {host}: run tea logins add")))
}

/// A login of tea.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct Login {
    name: String,
    url: String,
    /// The host of ssh, for the remotes in ssh.
    #[serde(default)]
    ssh_host: String,
}

fn parse_logins(text: &str) -> Result<Vec<Login>, Error> {
    serde_json::from_str(text)
        .map_err(|error| Error::Command(format!("tea logins list: invalid JSON: {error}")))
}

/// The name of the login of the host `host`, in this order: the host of
/// its URL with the port, its host of ssh, then the host of its URL
/// without the port. A remote in ssh has no port of the web.
fn login_for(logins: &[Login], host: &str) -> Option<String> {
    let find = |matches: &dyn Fn(&Login) -> bool| {
        logins
            .iter()
            .find(|login| matches(login))
            .map(|login| login.name.clone())
    };
    find(&|login| url_host(&login.url).eq_ignore_ascii_case(host))
        .or_else(|| find(&|login| login.ssh_host.eq_ignore_ascii_case(host)))
        .or_else(|| {
            find(&|login| {
                let url_host = url_host(&login.url);
                let without_port = url_host.split(':').next().unwrap_or(url_host);
                without_port.eq_ignore_ascii_case(host)
            })
        })
}

/// The host of an URL, with its port: `https://git.example:3000/sub` gives
/// `git.example:3000`.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split('/').next().unwrap_or_default();
    host.rsplit_once('@').map_or(host, |(_, host)| host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgejo_gives_the_pull_requests_of_the_branch() {
        let listed = parse_pulls(include_str!("../tests/fixtures/forgejo-pulls.json")).unwrap();

        let summary: Vec<(u64, PullRequestState, bool)> = listed
            .iter()
            .map(|pull| (pull.number, pull.state(), pull.is_of_repository()))
            .collect();
        assert_eq!(
            summary,
            [
                (15024, PullRequestState::Open, true),
                (9677, PullRequestState::Merged, true),
                (8190, PullRequestState::Merged, true),
                // A fork with the same branch name.
                (15300, PullRequestState::Open, false),
            ]
        );
        assert!(listed
            .iter()
            .all(|pull| pull.branch() == "renovate/forgejo-chart.js-4.x"));
        // The trap: a finished pull request gives the commit of the branch
        // now, the same as the open one.
        assert_eq!(listed[0].head.sha, listed[1].head.sha);
    }

    #[test]
    fn a_deleted_branch_keeps_its_name_in_the_label() {
        let pull = parse_pull(include_str!("../tests/fixtures/gitea-pull-by-branch.json")).unwrap();

        assert_eq!(pull.head.reference, "refs/pull/1117/head");
        assert_eq!(pull.branch(), "renovate/dependencies");
        assert_eq!(pull.state(), PullRequestState::Merged);
        assert!(pull.is_of_repository());

        // A fork gives `owner:branch` in its label.
        let fork = Listed {
            head: Side {
                label: "someone:feat/x".to_owned(),
                reference: "refs/pull/7/head".to_owned(),
                sha: None,
                repo_id: Some(2),
            },
            ..pull
        };
        assert_eq!(fork.branch(), "feat/x");
        assert!(!fork.is_of_repository());
    }

    #[test]
    fn gitea_gives_the_open_pull_requests_of_all_the_branches() {
        let listed = parse_pulls(include_str!("../tests/fixtures/gitea-pulls-open.json")).unwrap();

        let of_repository: Vec<(u64, &str)> = listed
            .iter()
            .filter(|pull| pull.is_of_repository())
            .map(|pull| (pull.number, pull.branch()))
            .collect();
        assert_eq!(listed.len(), 7);
        assert_eq!(of_repository, [(1210, "enhance/macos-vm-executor")]);
    }

    #[test]
    fn the_commits_come_the_newest_first() {
        let commits =
            parse_commits(include_str!("../tests/fixtures/forgejo-commits.json")).unwrap();

        assert_eq!(
            commits,
            [
                "4e4a0632a2a8b133eadef6cc6f6ab0dceed395f2",
                "50dd30d8502f3938fa17b353a598d9f4ed7c62ae",
                "afec2b0640db9faba069d57a3c17a73eed1a70af",
            ]
            .map(str::to_owned)
        );
    }

    #[test]
    fn an_error_of_the_api_is_not_a_list() {
        let not_found = r#"{"message":"not found","url":"https://gitea.com/api/swagger"}"#;

        assert!(parse_pulls(not_found).is_err());
        assert_eq!(error_message(not_found), "not found");
        assert_eq!(error_message("<html>"), "<html>");
    }

    #[test]
    fn the_status_line_gives_the_code() {
        assert_eq!(
            status_code("HTTP/2.0 200 OK\nContent-Type: json\n"),
            Some(200)
        );
        assert_eq!(status_code("HTTP/1.1 404 Not Found\n"), Some(404));
        assert_eq!(
            status_code("NOTE: something\nHTTP/1.1 500 Error\n"),
            Some(500)
        );
        assert_eq!(status_code("request failed"), None);
    }

    #[test]
    fn the_login_comes_from_the_host_of_its_url() {
        // The JSON of `tea logins list --output json`.
        let logins = parse_logins(
            r#"[
              {"name":"codeberg","url":"https://codeberg.org","ssh_host":"codeberg.org","user":"me","default":"true"},
              {"name":"work","url":"https://git.company.example:3000/gitea","ssh_host":"git.company.example","user":"me","default":"false"}
            ]"#,
        )
        .unwrap();

        assert_eq!(
            login_for(&logins, "codeberg.org"),
            Some("codeberg".to_owned())
        );
        assert_eq!(
            login_for(&logins, "git.company.example:3000"),
            Some("work".to_owned())
        );
        // A remote in ssh has no port of the web: the host of ssh gives
        // the login.
        assert_eq!(
            login_for(&logins, "git.company.example"),
            Some("work".to_owned())
        );
        assert_eq!(login_for(&logins, "gitea.com"), None);
    }

    #[test]
    fn a_login_without_ssh_host_matches_its_url_without_port() {
        let logins = parse_logins(r#"[{"name":"web","url":"https://git.example:3000"}]"#).unwrap();

        assert_eq!(login_for(&logins, "git.example"), Some("web".to_owned()));
    }

    #[test]
    fn a_head_without_repository_is_not_of_the_repository() {
        // Gitea gives -1 for a deleted fork or an AGit pull request.
        let pull = parse_pull(
            r#"{"number":4,"state":"open","merged":false,
                "head":{"label":"x","ref":"refs/pull/4/head","sha":"aaa","repo_id":-1},
                "base":{"label":"main","ref":"main","repo_id":-1}}"#,
        )
        .unwrap();

        assert!(!pull.is_of_repository());
    }

    #[test]
    fn the_headers_give_the_total_of_the_list() {
        let stderr = "HTTP/1.1 200 OK\nContent-Type: application/json\nX-Total-Count: 37\n\n";

        assert_eq!(total_count(stderr), Some(37));
        assert_eq!(total_count("HTTP/1.1 200 OK\n"), None);
        assert_eq!(
            parse_default_branch(r#"{"id":1,"default_branch":"main"}"#).unwrap(),
            "main"
        );
    }

    #[test]
    fn url_host_keeps_the_port_and_drops_the_user() {
        assert_eq!(url_host("https://codeberg.org"), "codeberg.org");
        assert_eq!(
            url_host("https://me@git.example:3000/sub"),
            "git.example:3000"
        );
        assert_eq!(url_host("git.example"), "git.example");
    }

    #[test]
    fn a_finished_pull_request_contains_the_commits_of_its_list() {
        let gitea = Gitea::new(
            "codeberg.org".to_owned(),
            "forgejo/forgejo".to_owned(),
            Flavor::Forgejo,
            Duration::from_secs(1),
        );
        lock(&gitea.commits).insert(
            "head".to_owned(),
            vec!["head".to_owned(), "local".to_owned()],
        );

        assert!(gitea.contains("head", "local"));
        assert!(!gitea.contains("head", "other"));
        assert!(!gitea.contains("unknown", "local"));
    }
}
