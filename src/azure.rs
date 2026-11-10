//! The calls to Azure DevOps, with the Azure CLI and its extension
//! `azure-devops`: `az repos pr list`.
//!
//! The CLI keeps the login (`az login`, or `az devops login` with a
//! personal access token). The backend never lets the CLI install its
//! extension: a missing extension gives an error.
//!
//! The proof of a finished pull request is its last commit only
//! (`lastMergeSourceCommit`): the backend does not read the commits of a
//! pull request. A commit on the forge after the local commit (for example
//! a suggestion that a review applies) keeps the worktree.

use std::env;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::forge::{self, Backend, PullRequest, PullRequestState, PAGE_SIZE};
use crate::herdr::Error;
use crate::process;

/// The backend of a repository on Azure DevOps.
pub struct AzureDevOps {
    /// The URL of the organization: `https://dev.azure.com/<organization>`.
    organization_url: String,
    project: String,
    repository: String,
    /// The time limit of each call to az.
    limit: Duration,
}

impl AzureDevOps {
    pub fn new(
        organization_url: String,
        project: String,
        repository: String,
        limit: Duration,
    ) -> AzureDevOps {
        AzureDevOps {
            organization_url,
            project,
            repository,
            limit,
        }
    }
}

impl Backend for AzureDevOps {
    fn pull_requests(&self, branch: &str) -> Result<Vec<PullRequest>, Error> {
        let mut command = Command::new("az");
        command
            .args([
                "repos",
                "pr",
                "list",
                "--org",
                &self.organization_url,
                "--project",
                &self.project,
                "--repository",
                &self.repository,
                "--source-branch",
                branch,
                "--status",
                "all",
                "--top",
                &PAGE_SIZE.to_string(),
                // Not the config of git in the folder of the process.
                "--detect",
                "false",
                "--output",
                "json",
            ])
            .env("AZURE_CORE_ONLY_SHOW_ERRORS", "true")
            .env("AZURE_CORE_NO_COLOR", "true")
            .env("AZURE_CORE_COLLECT_TELEMETRY", "false")
            // The observer runs in the background: it must not install
            // software.
            .env("AZURE_EXTENSION_USE_DYNAMIC_INSTALL", "no")
            .current_dir(env::temp_dir());
        let text = process::run_cli(&mut command, "az repos pr list", self.limit)?;
        parse_pull_requests(&text, branch)
    }

    /// Only the last commit proves a finished pull request.
    fn contains(&self, _head: &str, _commit: &str) -> bool {
        false
    }

    /// git keeps the local commit until its next `git gc`. Azure DevOps
    /// gives the commit of a completed pull request by its hash, also
    /// after the deletion of the branch: not tested for an abandoned one.
    fn recover_command(&self, _pull_request: &PullRequest, commit: &str, branch: &str) -> String {
        format!("git branch {branch} {commit} || git fetch origin {commit}:refs/heads/{branch}")
    }

    fn reference(&self, number: u64) -> String {
        format!("pull request !{number}")
    }
}

/// Read the JSON of `az repos pr list`: the pull requests of `branch`. A
/// pull request from a fork does not count. A status that the plugin does
/// not know keeps the worktree, as an open pull request.
fn parse_pull_requests(text: &str, branch: &str) -> Result<Vec<PullRequest>, Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Commit {
        commit_id: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Listed {
        pull_request_id: u64,
        status: String,
        #[serde(default)]
        source_ref_name: String,
        last_merge_source_commit: Option<Commit>,
        fork_source: Option<serde_json::Value>,
    }
    let listed: Vec<Listed> = forge::parse_page(text, "az repos pr list")?;
    let source = format!("refs/heads/{branch}");
    Ok(listed
        .into_iter()
        .filter(|listed| listed.fork_source.is_none() && listed.source_ref_name == source)
        .map(|listed| PullRequest {
            number: listed.pull_request_id,
            state: match listed.status.as_str() {
                "completed" => PullRequestState::Merged,
                "abandoned" => PullRequestState::Closed,
                // "active", and the states that the plugin does not know.
                _ => PullRequestState::Open,
            },
            head: listed
                .last_merge_source_commit
                .map(|commit| commit.commit_id)
                .unwrap_or_default(),
        })
        .collect())
}

/// The name of a host, without its port and in lowercase.
fn host_name(host: &str) -> String {
    host.split(':').next().unwrap_or(host).to_ascii_lowercase()
}

/// The URL of the organization (or of the collection, on Azure DevOps
/// Server) of a host.
pub fn organization_url(host: &str, organization: &str) -> String {
    let name = host_name(host);
    if name == "dev.azure.com" || name == "ssh.dev.azure.com" {
        format!("https://dev.azure.com/{organization}")
    } else if name.ends_with(".visualstudio.com") {
        format!("https://{organization}.visualstudio.com")
    } else {
        format!("https://{host}/{organization}")
    }
}

/// The organization, the project and the repository of the path of a
/// remote on Azure DevOps, with the host of the remote:
///
/// - ssh (`ssh.dev.azure.com`, `vs-ssh.visualstudio.com`):
///   `v3/<organization>/<project>/<repository>`
/// - `dev.azure.com`: `<organization>/[<project>/]_git/<repository>`
/// - `<organization>.visualstudio.com`:
///   `[DefaultCollection/][<project>/]_git/<repository>`
/// - Azure DevOps Server: `<collection>/[<project>/]_git/<repository>`
///
/// Without a project in the path, the project has the name of the
/// repository. The names come back decoded: `My%20Project` gives
/// `My Project`.
pub fn parse_remote_path(host: &str, path: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let name = host_name(host);
    let (organization, project, repository) = if name == "ssh.dev.azure.com"
        || name == "vs-ssh.visualstudio.com"
    {
        match parts.as_slice() {
            ["v3", organization, project, repository] => {
                (*organization, Some(*project), *repository)
            }
            _ => return None,
        }
    } else {
        let (before, repository) = match parts.as_slice() {
            [before @ .., "_git", repository] => (before, *repository),
            _ => return None,
        };
        if let Some(organization) = name.strip_suffix(".visualstudio.com") {
            let before = match before {
                [collection, rest @ ..] if collection.eq_ignore_ascii_case("DefaultCollection") => {
                    rest
                }
                _ => before,
            };
            match before {
                [] => (organization, None, repository),
                [project] => (organization, Some(*project), repository),
                _ => return None,
            }
        } else {
            // dev.azure.com, or a collection of Azure DevOps Server.
            match before {
                [organization] => (*organization, None, repository),
                [organization, project] => (*organization, Some(*project), repository),
                _ => return None,
            }
        }
    };
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    Some((
        percent_decode(organization),
        percent_decode(project.unwrap_or(repository)),
        percent_decode(repository),
    ))
}

/// Decode the `%XX` of an URL. A `%` without 2 hexadecimal digits stays.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_json_of_az_gives_the_pull_requests_of_the_branch() {
        let text = include_str!("../tests/fixtures/azure-pull-requests.json");

        let pull_requests = parse_pull_requests(text, "docs-update").unwrap();

        assert_eq!(
            pull_requests,
            [
                PullRequest {
                    number: 1,
                    state: PullRequestState::Merged,
                    head: "cd6039e649dcf41c410d9e970d391165cf9b8124".to_owned(),
                },
                PullRequest {
                    number: 7,
                    state: PullRequestState::Open,
                    head: "1111111111111111111111111111111111111111".to_owned(),
                },
                PullRequest {
                    number: 8,
                    state: PullRequestState::Closed,
                    head: "2222222222222222222222222222222222222222".to_owned(),
                },
            ]
        );
        // Another branch, and the fork !9, do not count.
        assert_eq!(
            parse_pull_requests(text, "invBootstrap").unwrap()[0].number,
            5
        );
        assert!(parse_pull_requests("{\"message\":\"x\"}", "main").is_err());
    }

    #[test]
    fn each_form_of_remote_gives_the_organization_the_project_and_the_repository() {
        let expected = Some((
            "myorg".to_owned(),
            "My Project".to_owned(),
            "my-repo".to_owned(),
        ));
        assert_eq!(
            parse_remote_path("dev.azure.com", "myorg/My%20Project/_git/my-repo"),
            expected
        );
        assert_eq!(
            parse_remote_path("ssh.dev.azure.com", "v3/myorg/My%20Project/my-repo"),
            expected
        );
        assert_eq!(
            parse_remote_path("myorg.visualstudio.com", "My%20Project/_git/my-repo"),
            expected
        );
        assert_eq!(
            parse_remote_path(
                "myorg.visualstudio.com",
                "DefaultCollection/My%20Project/_git/my-repo.git"
            ),
            expected
        );
        assert_eq!(parse_remote_path("dev.azure.com", "myorg/my-repo"), None);
        // Old ssh, a lowercase collection, a port, an encoded name.
        assert_eq!(
            parse_remote_path("vs-ssh.visualstudio.com", "v3/myorg/My%20Project/my-repo"),
            expected
        );
        assert_eq!(
            parse_remote_path(
                "myorg.visualstudio.com",
                "defaultcollection/My%20Project/_git/my-repo"
            ),
            expected
        );
        assert_eq!(
            parse_remote_path("dev.azure.com:443", "myorg/My%20Project/_git/my-repo"),
            expected
        );
        assert_eq!(
            parse_remote_path("dev.azure.com", "myorg/My%20Project/_git/my%20repo"),
            Some((
                "myorg".to_owned(),
                "My Project".to_owned(),
                "my repo".to_owned()
            ))
        );
    }

    #[test]
    fn a_path_without_project_names_the_project_after_the_repository() {
        let expected = Some((
            "myorg".to_owned(),
            "my-repo".to_owned(),
            "my-repo".to_owned(),
        ));

        assert_eq!(
            parse_remote_path("dev.azure.com", "myorg/_git/my-repo"),
            expected
        );
        assert_eq!(
            parse_remote_path("myorg.visualstudio.com", "_git/my-repo"),
            expected
        );
    }

    #[test]
    fn azure_devops_server_has_a_collection() {
        assert_eq!(
            parse_remote_path("tfs.example.com", "DefaultCollection/Project/_git/repo"),
            Some((
                "DefaultCollection".to_owned(),
                "Project".to_owned(),
                "repo".to_owned()
            ))
        );
        assert_eq!(
            organization_url("tfs.example.com:8080", "DefaultCollection"),
            "https://tfs.example.com:8080/DefaultCollection"
        );
    }

    #[test]
    fn the_organization_url_depends_on_the_host() {
        assert_eq!(
            organization_url("dev.azure.com", "myorg"),
            "https://dev.azure.com/myorg"
        );
        assert_eq!(
            organization_url("myorg.visualstudio.com", "myorg"),
            "https://myorg.visualstudio.com"
        );
        assert_eq!(
            organization_url("vs-ssh.visualstudio.com", "myorg"),
            "https://myorg.visualstudio.com"
        );
    }

    #[test]
    fn percent_decode_keeps_an_invalid_sequence() {
        assert_eq!(percent_decode("My%20Project"), "My Project");
        assert_eq!(percent_decode("%C3%A9t%C3%A9"), "été");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%+A"), "%+A");
    }

    #[test]
    fn azure_writes_a_pull_request_with_an_exclamation_mark() {
        let azure = AzureDevOps::new(
            "https://dev.azure.com/myorg".to_owned(),
            "p".to_owned(),
            "r".to_owned(),
            Duration::from_secs(1),
        );
        let merged = PullRequest {
            number: 5,
            state: PullRequestState::Merged,
            head: "aaa".to_owned(),
        };

        assert_eq!(azure.reference(5), "pull request !5");
        assert_eq!(
            azure.recover_command(&merged, "aaa", "feat/x"),
            "git branch feat/x aaa || git fetch origin aaa:refs/heads/feat/x"
        );
        assert!(!azure.contains("aaa", "aaa"));
    }

    #[test]
    fn a_full_page_gives_an_error() {
        let page = crate::test_support::full_page(|number| {
            format!(
                r#"{{"pullRequestId":{number},"status":"completed","sourceRefName":"refs/heads/develop"}}"#
            )
        });

        assert!(parse_pull_requests(&page, "develop").is_err());
    }
}
