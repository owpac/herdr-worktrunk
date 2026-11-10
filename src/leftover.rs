//! The facts of a worktree, layer by layer, and the verdict of the
//! cleaner: to delete, or kept.
//!
//! The layers, from the cheapest to the most specific:
//!
//! 1. Agent (herdr): an agent works in the worktree.
//! 2. Changes (git status): changes that are not committed.
//! 3. Integration (wt): the content of the branch is in the default
//!    branch, for example after a merge or a squash merge. Any forge.
//! 4. Push (git): a branch that was never pushed has no pull request. A
//!    deleted remote branch is a hint only.
//! 5. Pull requests (the backend of the forge): open, merged or closed, and
//!    the proof that a finished pull request contains the last commit.
//! 6. Activity (git): the days since the last commit or checkout.
//!
//! The verdict, in this order:
//!
//! 1. An agent, or changes: kept.
//! 2. An open pull request: kept.
//! 3. A finished pull request (merged or closed) that contains the last
//!    commit: to delete.
//! 4. The content is in the default branch, and no activity for 1 day: to
//!    delete.
//! 5. No pull request (the forge says so, or the branch was never pushed),
//!    and no activity for `stale_days` days: to delete.
//! 6. Else kept: recent activity, work after a finished pull request, or
//!    a forge that gave no answer.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prune::{BranchStatus, KeptWhy};
use crate::style::{ACCENT, DIM, GREEN, RED, YELLOW};

/// The days without activity before an integrated worktree is a
/// leftover: a new worktree has the commit of the default branch, so it
/// is integrated at once. `wt step prune` has the same rule.
pub const INTEGRATED_MIN_DAYS: u64 = 1;

/// The result of `git status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Changes {
    #[default]
    NotChecked,
    None,
    Some,
    /// git status failed.
    Failed,
}

/// The integration of the branch in the default branch, from wt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Integration {
    #[default]
    NotChecked,
    NotIntegrated,
    /// The check of wt that found the content in the default branch, for
    /// example `patch_id_match` for a squash merge.
    Integrated(String),
    /// wt could not decide (a time limit, an orphan branch).
    Undetermined,
}

/// Where the branch is on the remotes, from git.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Push {
    #[default]
    NotChecked,
    /// No upstream, and no remote branch of the same name.
    Never,
    /// The remote branch (`origin/feat`).
    Pushed(String),
    /// The upstream is gone: the last fetch removed it. A hint only.
    Gone(String),
}

/// The pull requests of the branch, from the backend of the forge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullRequests {
    #[default]
    NotChecked,
    /// The checks did not ask the forge, for this reason.
    NotNeeded(String),
    None,
    Open(u64),
    /// A finished pull request that contains the last commit.
    Finished {
        number: u64,
        merged: bool,
    },
    /// Finished, but the branch has commits after it.
    WorkAfter(u64),
    /// Finished, and the default branch contains the commit.
    InDefault(u64),
    /// Finished, but the branch has no local commit.
    NoLocalCommit(u64),
    /// The forge gave no answer, for this reason.
    Failed(String),
}

impl PullRequests {
    /// The facts of a status of the forge.
    pub fn from_status(status: Result<BranchStatus, String>) -> PullRequests {
        match status {
            Err(error) => PullRequests::Failed(error),
            Ok(BranchStatus::NoPullRequest) => PullRequests::None,
            Ok(BranchStatus::Open(number)) => PullRequests::Open(number),
            Ok(BranchStatus::Finished { pull_request, .. }) => PullRequests::Finished {
                number: pull_request.number,
                merged: pull_request.state == crate::github::PullRequestState::Merged,
            },
            Ok(BranchStatus::Kept { number, why }) => match why {
                KeptWhy::WorkAfter => PullRequests::WorkAfter(number),
                KeptWhy::InDefaultBranch => PullRequests::InDefault(number),
                KeptWhy::NoLocalCommit => PullRequests::NoLocalCommit(number),
            },
        }
    }

    /// The number of the pull request, if the facts name one.
    pub fn number(&self) -> Option<u64> {
        match self {
            PullRequests::Open(number)
            | PullRequests::Finished { number, .. }
            | PullRequests::WorkAfter(number)
            | PullRequests::InDefault(number)
            | PullRequests::NoLocalCommit(number) => Some(*number),
            _ => None,
        }
    }
}

/// The facts of a worktree, layer by layer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub agent: bool,
    pub changes: Changes,
    pub integration: Integration,
    pub push: Push,
    pub pull_requests: PullRequests,
    /// The forge that gave the pull requests, for the user.
    pub forge: String,
    /// The days since the last activity, once known.
    pub days: Option<u64>,
}

/// The verdict of the cleaner for a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// The checks run.
    Checking,
    /// To delete: a merged pull request contains the last commit.
    Merged,
    /// To delete: a closed pull request contains the last commit.
    Closed,
    /// To delete: the content is in the default branch.
    Integrated,
    /// To delete: no pull request, and no activity for `stale_days` days.
    Old,
    /// Kept: an open pull request.
    Open,
    /// Kept: some activity in the last days.
    Recent,
    /// Kept: changes that are not committed.
    Changes,
    /// Kept: an agent works in the worktree.
    Agent,
    /// Kept: the branch has work after its finished pull request.
    WorkAfter,
    /// Kept: a finished pull request, and the default branch contains the
    /// commit, but some activity today.
    InDefault,
    /// Kept: a check gave no answer.
    Unknown,
}

impl Reason {
    /// Return true for a leftover: the group "To delete".
    pub fn is_leftover(self) -> bool {
        matches!(
            self,
            Reason::Merged | Reason::Closed | Reason::Integrated | Reason::Old
        )
    }
}

/// The verdict for the facts of a worktree (see the module doc).
pub fn verdict(facts: &Facts, stale_days: u32) -> Reason {
    if facts.agent {
        return Reason::Agent;
    }
    match facts.changes {
        Changes::None => {}
        Changes::Some => return Reason::Changes,
        Changes::Failed | Changes::NotChecked => return Reason::Unknown,
    }
    match facts.pull_requests {
        PullRequests::Open(_) => return Reason::Open,
        PullRequests::Finished { merged: true, .. } => return Reason::Merged,
        PullRequests::Finished { merged: false, .. } => return Reason::Closed,
        _ => {}
    }
    let is_integrated = matches!(facts.integration, Integration::Integrated(_));
    if is_integrated && facts.days.is_some_and(|days| days >= INTEGRATED_MIN_DAYS) {
        return Reason::Integrated;
    }
    // A branch that was never pushed has no pull request, but only if the
    // forge gives no answer: a push without -u, or to another name, can
    // leave no remote branch here.
    let forge_is_silent = matches!(
        facts.pull_requests,
        PullRequests::Failed(_) | PullRequests::NotChecked | PullRequests::NotNeeded(_)
    );
    let has_no_pull_request = matches!(facts.pull_requests, PullRequests::None)
        || (matches!(facts.push, Push::Never) && forge_is_silent);
    if has_no_pull_request {
        return match facts.days {
            Some(days) if days >= u64::from(stale_days) => Reason::Old,
            Some(_) => Reason::Recent,
            None => Reason::Unknown,
        };
    }
    match facts.pull_requests {
        PullRequests::WorkAfter(_) => Reason::WorkAfter,
        PullRequests::InDefault(_) => Reason::InDefault,
        _ if is_integrated && facts.days.is_some() => Reason::Recent,
        _ => Reason::Unknown,
    }
}

/// A number of days: `1 day`, `7 days`.
fn days_text(days: u64) -> String {
    if days == 1 {
        "1 day".to_owned()
    } else {
        format!("{days} days")
    }
}

/// The words of a check of wt that found the content in the default
/// branch.
fn integration_words(reason: &str) -> &str {
    match reason {
        "same_commit" => "the same commit",
        "ancestor" => "an older commit of the default branch",
        "no_added_changes" => "no added change",
        "trees_match" => "the same files",
        "merge_adds_nothing" => "a merge adds nothing",
        "patch_id_match" => "a squash merge",
        other => other,
    }
}

/// The detail of a check that gave no answer, for the row and the
/// question.
pub fn unknown_detail(facts: &Facts) -> String {
    match (&facts.changes, &facts.pull_requests) {
        (Changes::Failed, _) => "git status failed".to_owned(),
        (_, PullRequests::Failed(error)) => error.clone(),
        (_, PullRequests::NoLocalCommit(_)) => "no local commit".to_owned(),
        _ if facts.days.is_none() => "no date of activity".to_owned(),
        _ => "no answer".to_owned(),
    }
}

/// The text of a verdict in a row of the list.
pub fn reason_text(reason: Reason, facts: &Facts) -> String {
    let number = facts
        .pull_requests
        .number()
        .map(|number| format!("#{number}"))
        .unwrap_or_default();
    let never_pushed = matches!(facts.push, Push::Never);
    match reason {
        Reason::Checking => "checking…".to_owned(),
        Reason::Merged => format!("PR {number} merged"),
        Reason::Closed => format!("PR {number} closed"),
        Reason::Integrated => match &facts.integration {
            Integration::Integrated(how) => {
                format!("in the default branch: {}", integration_words(how))
            }
            _ => "in the default branch".to_owned(),
        },
        Reason::Old if never_pushed => "never pushed, no recent activity".to_owned(),
        Reason::Old => "no PR, no recent activity".to_owned(),
        Reason::Open => format!("PR {number} open"),
        Reason::Recent if matches!(facts.integration, Integration::Integrated(_)) => {
            "in the default branch, activity today".to_owned()
        }
        Reason::Recent if never_pushed => "never pushed, recent activity".to_owned(),
        Reason::Recent => "no PR, recent activity".to_owned(),
        Reason::Changes => "changes not committed".to_owned(),
        Reason::Agent => "an agent works here".to_owned(),
        Reason::WorkAfter => format!("work after PR {number}"),
        Reason::InDefault => format!("PR {number} finished, commit in the default branch"),
        Reason::Unknown => format!("not checked: {}", unknown_detail(facts)),
    }
}

/// The text of a verdict in the question of the popup confirm.
pub fn question(reason: Reason, facts: &Facts) -> String {
    let number = facts
        .pull_requests
        .number()
        .map(|number| format!("#{number}"))
        .unwrap_or_default();
    let days = facts.days.unwrap_or(0);
    match reason {
        Reason::Merged => format!("Its pull request {number} is merged."),
        Reason::Closed => format!("Its pull request {number} is closed."),
        Reason::Integrated => "The default branch contains its content.".to_owned(),
        Reason::Old => format!("No pull request, and no change for {days} days."),
        Reason::Open => format!("Careful: its pull request {number} is open."),
        Reason::Recent => format!("No proof that it is finished, and a change {days} days ago."),
        Reason::Changes => "Careful: it has changes that are not committed.".to_owned(),
        Reason::Agent => "Careful: an agent works in it.".to_owned(),
        Reason::WorkAfter => format!("Careful: the branch has work after {number}."),
        Reason::InDefault => format!(
            "Its pull request {number} is finished, and the default branch contains its commit."
        ),
        Reason::Checking | Reason::Unknown => "The cleaner did not check it.".to_owned(),
    }
}

/// The result of a layer in the tab `checks`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The layer allows the deletion.
    Pass,
    /// The layer keeps the worktree.
    Fail,
    /// A fact that does not decide.
    Info,
    /// The check runs.
    Wait,
    /// The check gave no answer.
    Unknown,
}

impl Mark {
    pub fn glyph(self) -> (&'static str, &'static str) {
        match self {
            Mark::Pass => (GREEN, "✓"),
            Mark::Fail => (RED, "✗"),
            Mark::Info => (DIM, "–"),
            Mark::Wait => (ACCENT, "…"),
            Mark::Unknown => (YELLOW, "?"),
        }
    }
}

/// A line of the tab `checks`: the result, the layer, the fact, and the
/// source of the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub mark: Mark,
    pub layer: &'static str,
    pub text: String,
    pub source: String,
}

fn line(mark: Mark, layer: &'static str, text: impl Into<String>, source: &str) -> Line {
    Line {
        mark,
        layer,
        text: text.into(),
        source: source.to_owned(),
    }
}

/// The lines of the tab `checks`, one for each layer, in the order of the
/// checks.
pub fn lines(facts: &Facts, reason: Reason, stale_days: u32) -> Vec<Line> {
    let checking = reason == Reason::Checking;
    let waiting = |layer: &'static str, source: &str| {
        if checking {
            line(Mark::Wait, layer, "checking…", source)
        } else {
            line(Mark::Info, layer, "not needed", source)
        }
    };
    let agent = if facts.agent {
        line(Mark::Fail, "Agent", "an agent works here", "herdr")
    } else {
        line(Mark::Pass, "Agent", "none", "herdr")
    };
    let changes = match facts.changes {
        Changes::None => line(Mark::Pass, "Changes", "none", "git status"),
        Changes::Some => line(Mark::Fail, "Changes", "not committed", "git status"),
        Changes::Failed => line(Mark::Unknown, "Changes", "git status failed", "git status"),
        Changes::NotChecked => waiting("Changes", "git status"),
    };
    let integration = match &facts.integration {
        Integration::Integrated(how) => line(
            Mark::Pass,
            "Integration",
            format!("in the default branch: {}", integration_words(how)),
            "wt",
        ),
        Integration::NotIntegrated => {
            line(Mark::Info, "Integration", "not in the default branch", "wt")
        }
        Integration::Undetermined => line(Mark::Unknown, "Integration", "undetermined", "wt"),
        Integration::NotChecked => waiting("Integration", "wt"),
    };
    let push = match &facts.push {
        Push::Never => line(
            Mark::Pass,
            "Push",
            "never pushed: no pull request is possible",
            "git",
        ),
        Push::Pushed(remote) => line(Mark::Info, "Push", format!("pushed to {remote}"), "git"),
        Push::Gone(remote) => line(
            Mark::Info,
            "Push",
            format!("{remote} is deleted (a hint only)"),
            "git",
        ),
        Push::NotChecked => waiting("Push", "git"),
    };
    let forge = if facts.forge.is_empty() {
        "forge".to_owned()
    } else {
        facts.forge.clone()
    };
    let pull_requests = match &facts.pull_requests {
        PullRequests::None => line(Mark::Pass, "Pull requests", "none", &forge),
        PullRequests::Finished { number, merged } => line(
            Mark::Pass,
            "Pull requests",
            format!(
                "#{number} {}, with the last commit",
                if *merged { "merged" } else { "closed" }
            ),
            &forge,
        ),
        PullRequests::Open(number) => line(
            Mark::Fail,
            "Pull requests",
            format!("#{number} open"),
            &forge,
        ),
        PullRequests::WorkAfter(number) => line(
            Mark::Fail,
            "Pull requests",
            format!("#{number} finished, but the branch has commits after it"),
            &forge,
        ),
        PullRequests::InDefault(number) => line(
            Mark::Info,
            "Pull requests",
            format!("#{number} finished, the default branch has the commit"),
            &forge,
        ),
        PullRequests::NoLocalCommit(number) => line(
            Mark::Unknown,
            "Pull requests",
            format!("#{number} finished, but no local commit"),
            &forge,
        ),
        PullRequests::NotNeeded(why) => line(
            Mark::Info,
            "Pull requests",
            format!("not needed: {why}"),
            &forge,
        ),
        PullRequests::Failed(error) => line(
            Mark::Unknown,
            "Pull requests",
            format!("no answer: {error}"),
            &forge,
        ),
        PullRequests::NotChecked => waiting("Pull requests", &forge),
    };
    let activity = match facts.days {
        Some(days) => {
            // The rule of the integration has its own limit.
            let limit = if matches!(facts.integration, Integration::Integrated(_)) {
                INTEGRATED_MIN_DAYS
            } else {
                u64::from(stale_days)
            };
            let mark = if days >= limit {
                Mark::Pass
            } else if reason == Reason::Recent {
                Mark::Fail
            } else {
                Mark::Info
            };
            line(
                mark,
                "Activity",
                format!(
                    "{} without activity (limit: {})",
                    days_text(days),
                    days_text(limit)
                ),
                "git",
            )
        }
        None => waiting("Activity", "git"),
    };
    vec![agent, changes, integration, push, pull_requests, activity]
}

/// The integration of each branch, from the JSON of
/// `wt list --format json --branches`: `default_branch.integration` is
/// absent when wt found no integration, null when wt could not decide, and
/// an object with the reason when the default branch has the content.
pub fn parse_integrations(json: &Value) -> HashMap<String, Integration> {
    let mut integrations = HashMap::new();
    for item in json["items"].as_array().into_iter().flatten() {
        let Some(branch) = item["branch"].as_str() else {
            continue;
        };
        let default_branch = &item["default_branch"];
        let integration = match default_branch.get("integration") {
            _ if !default_branch.is_object() => Integration::Undetermined,
            None => Integration::NotIntegrated,
            Some(Value::Null) => Integration::Undetermined,
            Some(found) => {
                Integration::Integrated(found["reason"].as_str().unwrap_or("integrated").to_owned())
            }
        };
        integrations.insert(branch.to_owned(), integration);
    }
    integrations
}

/// The push state of each local branch, from 2 outputs of
/// `git for-each-ref`: the branches (`<branch>\t<upstream>\t<track>`),
/// and the remote branches (`<remote>/<branch>`).
pub fn parse_pushes(branches: &str, remote_branches: &str) -> HashMap<String, Push> {
    let remotes: HashSet<&str> = remote_branches.lines().map(str::trim).collect();
    let mut pushes = HashMap::new();
    for row in branches.lines() {
        let mut fields = row.split('\t');
        let Some(branch) = fields.next().filter(|branch| !branch.is_empty()) else {
            continue;
        };
        let upstream = fields.next().unwrap_or_default();
        let track = fields.next().unwrap_or_default();
        let push = if !upstream.is_empty() {
            if track.contains("gone") {
                Push::Gone(upstream.to_owned())
            } else {
                Push::Pushed(upstream.to_owned())
            }
        } else {
            // A push without -u gives no upstream, but a remote branch.
            match remotes.iter().find(|remote| {
                remote
                    .split_once('/')
                    .is_some_and(|(_, name)| name == branch)
            }) {
                Some(remote) => Push::Pushed((*remote).to_owned()),
                None => Push::Never,
            }
        };
        pushes.insert(branch.to_owned(), push);
    }
    pushes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            agent: false,
            changes: Changes::None,
            integration: Integration::NotIntegrated,
            push: Push::Pushed("origin/feat".to_owned()),
            pull_requests: PullRequests::None,
            forge: "GitHub".to_owned(),
            days: Some(30),
        }
    }

    #[test]
    fn the_verdict_follows_the_order_of_the_layers() {
        let integrated = Integration::Integrated("patch_id_match".to_owned());
        let finished = PullRequests::Finished {
            number: 3,
            merged: true,
        };
        let cases: Vec<(&str, Facts, Reason)> = vec![
            (
                "agent",
                Facts {
                    agent: true,
                    ..facts()
                },
                Reason::Agent,
            ),
            (
                "changes",
                Facts {
                    changes: Changes::Some,
                    ..facts()
                },
                Reason::Changes,
            ),
            (
                "git status failed",
                Facts {
                    changes: Changes::Failed,
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "an open PR wins over the integration",
                Facts {
                    pull_requests: PullRequests::Open(3),
                    integration: integrated.clone(),
                    ..facts()
                },
                Reason::Open,
            ),
            (
                "merged PR",
                Facts {
                    pull_requests: finished.clone(),
                    days: Some(0),
                    ..facts()
                },
                Reason::Merged,
            ),
            (
                "closed PR",
                Facts {
                    pull_requests: PullRequests::Finished {
                        number: 3,
                        merged: false,
                    },
                    ..facts()
                },
                Reason::Closed,
            ),
            (
                "integrated, no activity for 1 day, the forge failed",
                Facts {
                    integration: integrated.clone(),
                    pull_requests: PullRequests::Failed("gh: no end".to_owned()),
                    days: Some(1),
                    ..facts()
                },
                Reason::Integrated,
            ),
            (
                "a new worktree is integrated today: kept",
                Facts {
                    integration: Integration::Integrated("same_commit".to_owned()),
                    pull_requests: PullRequests::NotNeeded("no remote origin".to_owned()),
                    push: Push::Never,
                    days: Some(0),
                    ..facts()
                },
                Reason::Recent,
            ),
            ("no PR and old", facts(), Reason::Old),
            (
                "no PR and recent",
                Facts {
                    days: Some(2),
                    ..facts()
                },
                Reason::Recent,
            ),
            (
                "never pushed, no remote, and old",
                Facts {
                    push: Push::Never,
                    pull_requests: PullRequests::NotNeeded("no remote origin".to_owned()),
                    ..facts()
                },
                Reason::Old,
            ),
            (
                "never pushed here, but the forge has an open PR",
                Facts {
                    push: Push::Never,
                    pull_requests: PullRequests::Open(5),
                    ..facts()
                },
                Reason::Open,
            ),
            (
                "never pushed and the forge failed: the age decides",
                Facts {
                    push: Push::Never,
                    pull_requests: PullRequests::Failed("no backend for GitLab yet".to_owned()),
                    ..facts()
                },
                Reason::Old,
            ),
            (
                "the commit is in the default branch, but wt failed",
                Facts {
                    integration: Integration::Undetermined,
                    pull_requests: PullRequests::InDefault(4),
                    ..facts()
                },
                Reason::InDefault,
            ),
            (
                "integrated without a date of activity",
                Facts {
                    integration: Integration::Integrated("ancestor".to_owned()),
                    pull_requests: PullRequests::WorkAfter(4),
                    days: None,
                    ..facts()
                },
                Reason::WorkAfter,
            ),
            (
                "the forge failed: not checked",
                Facts {
                    pull_requests: PullRequests::Failed("gh: no end".to_owned()),
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "no backend: not checked",
                Facts {
                    pull_requests: PullRequests::Failed("no backend for GitLab yet".to_owned()),
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "a deleted remote branch is a hint only",
                Facts {
                    push: Push::Gone("origin/feat".to_owned()),
                    pull_requests: PullRequests::Failed("no backend for GitLab yet".to_owned()),
                    ..facts()
                },
                Reason::Unknown,
            ),
            (
                "work after a finished PR",
                Facts {
                    pull_requests: PullRequests::WorkAfter(3),
                    ..facts()
                },
                Reason::WorkAfter,
            ),
            (
                "no date of activity",
                Facts {
                    days: None,
                    ..facts()
                },
                Reason::Unknown,
            ),
        ];
        for (name, facts, expected) in cases {
            assert_eq!(verdict(&facts, 7), expected, "{name}");
        }
    }

    #[test]
    fn a_failed_forge_never_gives_a_leftover_without_a_proof_of_git() {
        for days in [Some(0), Some(400), None] {
            let facts = Facts {
                pull_requests: PullRequests::Failed("gh pr: no end after 15 s".to_owned()),
                days,
                ..facts()
            };

            assert!(!verdict(&facts, 7).is_leftover(), "{days:?}");
        }
    }

    #[test]
    fn the_facts_go_to_json_and_back() {
        let facts = Facts {
            integration: Integration::Integrated("patch_id_match".to_owned()),
            push: Push::Gone("origin/feat".to_owned()),
            pull_requests: PullRequests::Finished {
                number: 12,
                merged: false,
            },
            ..facts()
        };

        let text = serde_json::to_string(&facts).unwrap();

        assert_eq!(serde_json::from_str::<Facts>(&text).unwrap(), facts);
    }

    #[test]
    fn the_tab_checks_has_one_line_for_each_layer() {
        let facts = Facts {
            integration: Integration::Integrated("patch_id_match".to_owned()),
            days: Some(2),
            ..facts()
        };

        let shown = lines(&facts, Reason::Integrated, 7);

        let layers: Vec<&str> = shown.iter().map(|line| line.layer).collect();
        assert_eq!(
            layers,
            [
                "Agent",
                "Changes",
                "Integration",
                "Push",
                "Pull requests",
                "Activity"
            ]
        );
        assert_eq!(shown[2].text, "in the default branch: a squash merge");
        assert_eq!(shown[2].source, "wt");
        assert_eq!(shown[4].source, "GitHub");
        assert_eq!(shown[5].text, "2 days without activity (limit: 1 day)");
        assert_eq!(shown[5].mark, Mark::Pass);
    }

    #[test]
    fn an_integrated_worktree_kept_today_shows_the_limit_of_1_day() {
        let kept = Facts {
            integration: Integration::Integrated("same_commit".to_owned()),
            days: Some(0),
            ..facts()
        };

        let activity = &lines(&kept, Reason::Recent, 7)[5];

        assert_eq!(activity.text, "0 days without activity (limit: 1 day)");
        assert_eq!(activity.mark, Mark::Fail);
        assert_eq!(
            reason_text(
                Reason::InDefault,
                &Facts {
                    pull_requests: PullRequests::InDefault(4),
                    ..facts()
                }
            ),
            "PR #4 finished, commit in the default branch"
        );
    }

    #[test]
    fn a_running_check_shows_its_layers_as_waiting() {
        let facts = Facts {
            changes: Changes::NotChecked,
            integration: Integration::NotChecked,
            push: Push::NotChecked,
            pull_requests: PullRequests::NotChecked,
            days: None,
            ..facts()
        };

        let marks: Vec<Mark> = lines(&facts, Reason::Checking, 7)
            .iter()
            .map(|line| line.mark)
            .collect();

        assert_eq!(
            marks,
            [
                Mark::Pass,
                Mark::Wait,
                Mark::Wait,
                Mark::Wait,
                Mark::Wait,
                Mark::Wait
            ]
        );
    }

    #[test]
    fn the_json_of_wt_gives_the_integration_of_each_branch() {
        let json: Value =
            serde_json::from_str(include_str!("../tests/fixtures/wt-list.json")).unwrap();

        let integrations = parse_integrations(&json);

        assert_eq!(
            integrations["feat-squashed"],
            Integration::Integrated("patch_id_match".to_owned())
        );
        assert_eq!(integrations["feat-open"], Integration::NotIntegrated);
        assert_eq!(integrations["feat-orphan"], Integration::Undetermined);
        // The main checkout has no default_branch object.
        assert_eq!(integrations["main"], Integration::Undetermined);
    }

    #[test]
    fn the_push_state_comes_from_the_upstream_and_the_remote_branches() {
        let branches = "feat-a\torigin/feat-a\t\nfeat-gone\torigin/feat-gone\t[gone]\nfeat-no-u\t\t\nfeat-local\t\t\n";
        let remote_branches = "origin/HEAD\norigin/feat-a\norigin/feat-no-u\n";

        let pushes = parse_pushes(branches, remote_branches);

        assert_eq!(pushes["feat-a"], Push::Pushed("origin/feat-a".to_owned()));
        assert_eq!(
            pushes["feat-gone"],
            Push::Gone("origin/feat-gone".to_owned())
        );
        assert_eq!(
            pushes["feat-no-u"],
            Push::Pushed("origin/feat-no-u".to_owned())
        );
        assert_eq!(pushes["feat-local"], Push::Never);
    }

    #[test]
    fn the_row_tells_the_proof_of_the_verdict() {
        let squashed = Facts {
            integration: Integration::Integrated("patch_id_match".to_owned()),
            ..facts()
        };
        let never = Facts {
            push: Push::Never,
            ..facts()
        };

        assert_eq!(
            reason_text(Reason::Integrated, &squashed),
            "in the default branch: a squash merge"
        );
        assert_eq!(
            reason_text(Reason::Old, &never),
            "never pushed, no recent activity"
        );
        assert_eq!(
            reason_text(
                Reason::Unknown,
                &Facts {
                    pull_requests: PullRequests::Failed("no backend for GitLab yet".to_owned()),
                    ..facts()
                }
            ),
            "not checked: no backend for GitLab yet"
        );
    }
}
