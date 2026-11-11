//! The worktrees that the plugin removed, and their restore (action
//! restore).
//!
//! Before each removal (cleaner, actions remove and merge, observer), the
//! plugin records the worktree in `removed.jsonl` of the state folder: its
//! repository, its folder, its branch, the commit of its branch, its
//! agents with their sessions, and the layout of its herdr workspace: the
//! tabs, their labels, their splits, and the folder of each pane. The 20
//! last removals stay. The action restore lists them (see `restorer`), and
//! brings back the chosen one:
//!
//! 1. It creates the branch again on its commit, if the branch is gone.
//! 2. `wt switch` creates the worktree again. The hooks of wt open its
//!    herdr workspace.
//! 3. It creates the same tabs and splits again, with a shell in the same
//!    folder, and resumes each agent in its pane: `claude --resume
//!    <session>` or `codex resume <session>`.
//!
//! A program that ran in a pane, and a file that an editor did not save,
//! do not come back. The observer keeps a restored worktree for 1 day,
//! while its branch has the restored commit (`restored.jsonl`): else it
//! removes a finished worktree again at once.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::herdr::{self, Pane};
use crate::stale;

/// The number of removals that the file keeps.
const MAX_REMOVALS: usize = 20;

/// An agent of a removed worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedAgent {
    /// The kind of the agent: claude, codex...
    pub kind: String,
    pub session: String,
}

impl RemovedAgent {
    /// The agent of `kind` with `session`, or None without a session: it
    /// cannot come back.
    pub fn of(kind: &str, session: Option<&str>) -> Option<RemovedAgent> {
        Some(RemovedAgent {
            kind: kind.to_owned(),
            session: session.filter(|session| !session.is_empty())?.to_owned(),
        })
    }

    /// The arguments that resume the conversation, after the name of the
    /// agent, or None for a kind that the plugin cannot resume.
    pub fn resume_arguments(&self) -> Option<Vec<String>> {
        match self.kind.as_str() {
            "claude" => Some(vec!["--resume".to_owned(), self.session.clone()]),
            "codex" => Some(vec!["resume".to_owned(), self.session.clone()]),
            _ => None,
        }
    }

    /// A herdr name for the resumed agent: `[a-z][a-z0-9_-]`, 32
    /// characters or less.
    pub fn herdr_name(&self) -> String {
        let session: String = self
            .session
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|character| character.to_ascii_lowercase())
            .take(8)
            .collect();
        format!("restored-{session}")
    }
}

/// A rectangle of the layout of a tab, in cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn area(&self) -> f64 {
        self.width * self.height
    }

    fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// The 2 parts of a split: the first part keeps the pane, the second
    /// part gets the new pane (on the right, or below).
    fn split(&self, direction: &str, ratio: f64) -> (Rect, Rect) {
        if direction == "down" {
            let first = self.height * ratio;
            (
                Rect {
                    height: first,
                    ..*self
                },
                Rect {
                    y: self.y + first,
                    height: self.height - first,
                    ..*self
                },
            )
        } else {
            let first = self.width * ratio;
            (
                Rect {
                    width: first,
                    ..*self
                },
                Rect {
                    x: self.x + first,
                    width: self.width - first,
                    ..*self
                },
            )
        }
    }
}

/// A pane of a removed tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemovedPane {
    pub rect: Rect,
    /// The folder of the shell of the pane.
    pub cwd: PathBuf,
    /// The agent of the pane, if it can come back.
    pub agent: Option<RemovedAgent>,
}

/// A split of a removed tab: `right` or `down`, with the part of the first
/// pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemovedSplit {
    pub direction: String,
    pub ratio: f64,
    pub rect: Rect,
}

/// A tab of the herdr workspace of a removed worktree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemovedTab {
    pub label: String,
    pub panes: Vec<RemovedPane>,
    pub splits: Vec<RemovedSplit>,
}

/// One split of the rebuild of a tab: split the pane `parent` (its index
/// in `TabPlan::panes`); the new pane gets the index `child`.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitStep {
    pub parent: usize,
    pub child: usize,
    pub direction: String,
    pub ratio: f64,
}

/// The rebuild of a tab: the panes (the first one is the pane of the new
/// tab), and the splits, in their order.
#[derive(Clone, Debug, PartialEq)]
pub struct TabPlan {
    pub panes: Vec<Option<RemovedPane>>,
    pub steps: Vec<SplitStep>,
}

/// Plan the rebuild of `tab`. The largest split comes first: it splits the
/// whole tab. The pane of a split keeps the first part, and the new pane
/// gets the second part. At the end, each part gets the removed pane whose
/// center is in it.
pub fn plan_tab(tab: &RemovedTab) -> TabPlan {
    let area = tab
        .splits
        .iter()
        .map(|split| split.rect)
        .chain(tab.panes.iter().map(|pane| pane.rect))
        .fold(None::<Rect>, |largest, rect| match largest {
            Some(largest) if largest.area() >= rect.area() => Some(largest),
            _ => Some(rect),
        })
        .unwrap_or_default();
    let mut regions = vec![area];
    let mut steps = Vec::new();
    let mut splits: Vec<&RemovedSplit> = tab.splits.iter().collect();
    splits.sort_by(|a, b| b.rect.area().total_cmp(&a.rect.area()));
    for split in splits {
        let center = split.rect.center();
        let Some(parent) = regions.iter().position(|region| region.contains(center)) else {
            continue;
        };
        let (first, second) = regions[parent].split(&split.direction, split.ratio);
        regions[parent] = first;
        regions.push(second);
        steps.push(SplitStep {
            parent,
            child: regions.len() - 1,
            direction: split.direction.clone(),
            ratio: split.ratio,
        });
    }
    let panes = regions
        .iter()
        .map(|region| {
            tab.panes
                .iter()
                .find(|pane| region.contains(pane.rect.center()))
                .cloned()
        })
        .collect();
    TabPlan { panes, steps }
}

/// The tabs of the workspace `workspace_id`, for a record. An error gives
/// no tab: the restore then opens the agents in new tabs.
pub fn capture_tabs(workspace_id: &str) -> Vec<RemovedTab> {
    let (Ok(snapshot), Ok(tabs)) = (herdr::snapshot(), herdr::tabs(workspace_id)) else {
        return Vec::new();
    };
    tabs.iter()
        .filter_map(|tab| {
            let panes: Vec<&Pane> = snapshot
                .panes
                .iter()
                .filter(|pane| pane.tab_id == tab.tab_id)
                .collect();
            let layout = herdr::pane_layout(&panes.first()?.pane_id).ok()?;
            Some(tab_of_layout(&tab.label, &layout, &panes))
        })
        .collect()
}

/// A rectangle of the JSON of herdr.
fn rect_of(json: &serde_json::Value) -> Rect {
    let number = |name: &str| json[name].as_f64().unwrap_or(0.0);
    Rect {
        x: number("x"),
        y: number("y"),
        width: number("width"),
        height: number("height"),
    }
}

/// A removed tab from the layout of herdr (`pane layout`) and the panes of
/// the snapshot. Only the panes of the layout count: a popup of a plugin
/// is not in it.
fn tab_of_layout(label: &str, layout: &serde_json::Value, panes: &[&Pane]) -> RemovedTab {
    let removed_panes = layout["panes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let pane_id = item["pane_id"].as_str()?;
            let pane = panes.iter().find(|pane| pane.pane_id == pane_id)?;
            Some(RemovedPane {
                rect: rect_of(&item["rect"]),
                cwd: pane.folder()?.to_owned(),
                agent: pane.agent.as_deref().and_then(|kind| {
                    RemovedAgent::of(
                        kind,
                        pane.agent_session
                            .as_ref()
                            .map(|session| session.value.as_str()),
                    )
                }),
            })
        })
        .collect();
    let splits = layout["splits"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|split| RemovedSplit {
            direction: split["direction"].as_str().unwrap_or("right").to_owned(),
            ratio: split["ratio"].as_f64().unwrap_or(0.5),
            rect: rect_of(&split["rect"]),
        })
        .collect();
    RemovedTab {
        label: label.to_owned(),
        panes: removed_panes,
        splits,
    }
}

/// A removed worktree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Removal {
    /// The time of the removal, in seconds since 1970.
    pub time: u64,
    pub main_checkout: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// The commit of the branch at the removal.
    pub commit: String,
    pub agents: Vec<RemovedAgent>,
    /// The command that gets the branch back from the forge, for a closed
    /// pull request.
    #[serde(default)]
    pub recover_command: Option<String>,
    /// The tabs of the herdr workspace of the worktree, in their order.
    #[serde(default)]
    pub tabs: Vec<RemovedTab>,
}

impl Removal {
    /// The agents that are not in a pane of `tabs`: they come back in a new
    /// tab each.
    pub fn agents_without_pane(&self) -> Vec<&RemovedAgent> {
        self.agents
            .iter()
            .filter(|agent| {
                !self
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.agent.as_ref() == Some(*agent))
            })
            .collect()
    }
}

fn removed_path(state_dir: &Path) -> PathBuf {
    state_dir.join("removed.jsonl")
}

fn restored_path(state_dir: &Path) -> PathBuf {
    state_dir.join("restored.jsonl")
}

/// Read the lines of a JSON lines file. A line that is not valid stays
/// out.
fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn write_lines<T: Serialize>(path: &Path, items: &[T]) {
    let text: String = items
        .iter()
        .filter_map(|item| serde_json::to_string(item).ok())
        .map(|line| format!("{line}\n"))
        .collect();
    // A failed write loses only the restore of a removal.
    let _ = stale::write_atomically(path, &text);
}

/// Record a removal, before it starts: its agents are still there. A new
/// record of the same worktree replaces the older one: a removal that
/// failed comes again at the next pass of the observer.
pub fn record(state_dir: &Path, removal: &Removal) {
    let mut removals: Vec<Removal> = read_lines(&removed_path(state_dir));
    removals.retain(|other| other.path != removal.path);
    removals.push(removal.clone());
    let excess = removals.len().saturating_sub(MAX_REMOVALS);
    write_lines(&removed_path(state_dir), &removals[excess..]);
}

/// All the removals of the file, the oldest first.
pub fn all(state_dir: &Path) -> Vec<Removal> {
    read_lines(&removed_path(state_dir))
}

/// Remove `removal` from the file: it came back, or its worktree is there.
pub fn forget(state_dir: &Path, removal: &Removal) {
    let removals: Vec<Removal> = read_lines(&removed_path(state_dir));
    let kept: Vec<Removal> = removals
        .into_iter()
        .filter(|other| other != removal)
        .collect();
    write_lines(&removed_path(state_dir), &kept);
}

/// A restored worktree, that the observer keeps for 1 day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Restored {
    path: PathBuf,
    commit: String,
    time: u64,
}

/// Record a restored worktree, with the commit of its branch.
pub fn mark_restored(state_dir: &Path, path: &Path, commit: &str) {
    let now = stale::now();
    let mut restored: Vec<Restored> = read_lines(&restored_path(state_dir));
    restored.retain(|item| item.path != path && now.saturating_sub(item.time) < stale::DAY);
    restored.push(Restored {
        path: path.to_owned(),
        commit: commit.to_owned(),
        time: now,
    });
    write_lines(&restored_path(state_dir), &restored);
}

/// Return true if the observer must keep the worktree `path`: a restore of
/// less than 1 day, and its branch still has the restored commit.
pub fn is_kept(state_dir: &Path, path: &Path, commit: &str) -> bool {
    let now = stale::now();
    read_lines::<Restored>(&restored_path(state_dir))
        .iter()
        .any(|item| {
            item.path == path && item.commit == commit && now.saturating_sub(item.time) < stale::DAY
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_folder;

    fn removal(branch: &str, agents: Vec<RemovedAgent>) -> Removal {
        Removal {
            time: 10,
            main_checkout: PathBuf::from("/work/app"),
            path: PathBuf::from(format!("/work/app.{branch}")),
            branch: branch.to_owned(),
            commit: "abc123".to_owned(),
            agents,
            recover_command: None,
            tabs: Vec::new(),
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn pane(rect: Rect, cwd: &str, agent: Option<&str>) -> RemovedPane {
        RemovedPane {
            rect,
            cwd: PathBuf::from(cwd),
            agent: agent.map(|session| RemovedAgent {
                kind: "claude".to_owned(),
                session: session.to_owned(),
            }),
        }
    }

    #[test]
    fn the_layout_of_herdr_gives_a_removed_tab() {
        let layout = serde_json::json!({
            "panes": [
                {"pane_id": "w1:p1", "rect": {"x": 0, "y": 0, "width": 99, "height": 55}},
                {"pane_id": "w1:p2", "rect": {"x": 99, "y": 0, "width": 98, "height": 55}}
            ],
            "splits": [{"direction": "right", "ratio": 0.5, "rect": {"x": 0, "y": 0, "width": 197, "height": 55}}]
        });
        let panes: Vec<Pane> = serde_json::from_value(serde_json::json!([
            {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "agent": "claude",
             "agent_session": {"value": "s1"}, "cwd": "/work/app.feat", "foreground_cwd": null},
            {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1", "agent": null,
             "cwd": "/work/app.feat/src", "foreground_cwd": null},
            {"pane_id": "w1:p9", "workspace_id": "w1", "tab_id": "w1:t1", "agent": null,
             "cwd": "/tmp", "foreground_cwd": null}
        ]))
        .unwrap();
        let panes: Vec<&Pane> = panes.iter().collect();

        let tab = tab_of_layout("work", &layout, &panes);

        assert_eq!(tab.label, "work");
        assert_eq!(tab.panes.len(), 2, "the popup w1:p9 is not in the layout");
        assert_eq!(tab.panes[0].agent.as_ref().unwrap().session, "s1");
        assert_eq!(tab.panes[1].cwd, PathBuf::from("/work/app.feat/src"));
        assert_eq!(tab.splits[0].direction, "right");
    }

    #[test]
    fn a_tab_with_one_pane_needs_no_split() {
        let tab = RemovedTab {
            label: "editor".to_owned(),
            panes: vec![pane(rect(0.0, 0.0, 197.0, 55.0), "/work/app.feat", None)],
            splits: vec![],
        };

        let plan = plan_tab(&tab);

        assert!(plan.steps.is_empty());
        assert_eq!(plan.panes.len(), 1);
        assert_eq!(
            plan.panes[0].as_ref().unwrap().cwd,
            PathBuf::from("/work/app.feat")
        );
    }

    #[test]
    fn the_plan_rebuilds_nested_splits_and_places_each_pane() {
        // Left: an agent. Right: a shell above, a shell in src below.
        let tab = RemovedTab {
            label: "work".to_owned(),
            panes: vec![
                pane(rect(0.0, 0.0, 99.0, 55.0), "/work/app.feat", Some("s1")),
                pane(rect(99.0, 0.0, 98.0, 27.0), "/work/app.feat", None),
                pane(rect(99.0, 27.0, 98.0, 28.0), "/work/app.feat/src", None),
            ],
            splits: vec![
                RemovedSplit {
                    direction: "down".to_owned(),
                    ratio: 0.5,
                    rect: rect(99.0, 0.0, 98.0, 55.0),
                },
                RemovedSplit {
                    direction: "right".to_owned(),
                    ratio: 0.5,
                    rect: rect(0.0, 0.0, 197.0, 55.0),
                },
            ],
        };

        let plan = plan_tab(&tab);

        assert_eq!(
            plan.steps
                .iter()
                .map(|step| (step.parent, step.child, step.direction.as_str()))
                .collect::<Vec<_>>(),
            [(0, 1, "right"), (1, 2, "down")]
        );
        let cwds: Vec<String> = plan
            .panes
            .iter()
            .map(|pane| pane.as_ref().unwrap().cwd.display().to_string())
            .collect();
        assert_eq!(
            cwds,
            ["/work/app.feat", "/work/app.feat", "/work/app.feat/src"]
        );
        assert_eq!(
            plan.panes[0]
                .as_ref()
                .unwrap()
                .agent
                .as_ref()
                .unwrap()
                .session,
            "s1"
        );
    }

    #[test]
    fn an_agent_without_a_pane_of_the_layout_gets_its_own_tab() {
        let mut removal = removal(
            "feat",
            vec![
                RemovedAgent {
                    kind: "claude".to_owned(),
                    session: "s1".to_owned(),
                },
                RemovedAgent {
                    kind: "codex".to_owned(),
                    session: "s2".to_owned(),
                },
            ],
        );
        removal.tabs = vec![RemovedTab {
            label: "work".to_owned(),
            panes: vec![pane(
                rect(0.0, 0.0, 10.0, 10.0),
                "/work/app.feat",
                Some("s1"),
            )],
            splits: vec![],
        }];

        let alone: Vec<&str> = removal
            .agents_without_pane()
            .iter()
            .map(|agent| agent.session.as_str())
            .collect();
        assert_eq!(alone, ["s2"]);
    }

    #[test]
    fn the_last_removal_comes_first_and_only_20_stay() {
        let folder = test_folder("removals");
        for index in 0..25 {
            record(&folder, &removal(&format!("b{index}"), vec![]));
        }

        let last_one = all(&folder).pop().unwrap();
        assert_eq!(last_one.branch, "b24");
        assert_eq!(read_lines::<Removal>(&removed_path(&folder)).len(), 20);

        forget(&folder, &last_one);
        assert_eq!(all(&folder).pop().unwrap().branch, "b23");
    }

    #[test]
    fn a_new_record_of_the_same_worktree_replaces_the_older_one() {
        let folder = test_folder("removal-again");
        record(&folder, &removal("feat", vec![]));
        record(&folder, &removal("other", vec![]));
        let mut again = removal("feat", vec![]);
        again.time = 20;
        record(&folder, &again);

        let removals: Vec<Removal> = read_lines(&removed_path(&folder));
        let branches: Vec<&str> = removals
            .iter()
            .map(|removal| removal.branch.as_str())
            .collect();
        assert_eq!(branches, ["other", "feat"]);
        assert_eq!(removals[1].time, 20);
    }

    #[test]
    fn an_agent_without_session_cannot_come_back() {
        assert_eq!(RemovedAgent::of("claude", None), None);
        assert_eq!(RemovedAgent::of("claude", Some("")), None);
        let claude = RemovedAgent::of("claude", Some("A1b2-C3d4-e5")).unwrap();
        let codex = RemovedAgent::of("codex", Some("019a")).unwrap();

        assert_eq!(
            claude.resume_arguments().unwrap(),
            ["--resume", "A1b2-C3d4-e5"]
        );
        assert_eq!(codex.resume_arguments().unwrap(), ["resume", "019a"]);
        assert_eq!(
            RemovedAgent::of("pi", Some("s"))
                .unwrap()
                .resume_arguments(),
            None
        );
        assert_eq!(claude.herdr_name(), "restored-a1b2c3d4");
    }

    #[test]
    fn the_observer_keeps_a_restored_worktree_while_its_commit_stays() {
        let folder = test_folder("restored");
        let path = Path::new("/work/app.feat");
        mark_restored(&folder, path, "abc123");

        assert!(is_kept(&folder, path, "abc123"));
        assert!(!is_kept(&folder, path, "def456"), "a new commit");
        assert!(!is_kept(&folder, Path::new("/work/app.other"), "abc123"));
    }
}
