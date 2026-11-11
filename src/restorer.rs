//! The popup restorer: the last removed worktrees in fzf, with the style
//! of the cleaner. The newest removal comes first. The preview tells what
//! comes back. `Enter` restores the chosen worktree.
//!
//! A row has 3 hidden fields: the time of the removal, the folder of the
//! worktree, and the main checkout. They name the record in
//! `removed.jsonl` (see `restore`).

use std::env;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::git;
use crate::herdr::Error;
use crate::picker::{shell_quote, FZF_CANCEL_EXIT_CODE, FZF_NO_MATCH_EXIT_CODE};
use crate::restore::{self, Removal};
use crate::stale;
use crate::style::{self, part, ACCENT, BOLD, DIM, GREEN, MUTED, REPOSITORY, RESET, RULE, YELLOW};

const GHOST: &str = "removed worktree — type to filter";
// The footer has the width of the list: the hints must be short.
const HINTS: [(&str, &str); 2] = [("⏎", "restore"), ("esc", "close")];

/// The time since `time`, short: `now`, `5m`, `3h`, `2d`.
fn ago(time: u64, now: u64) -> String {
    let seconds = now.saturating_sub(time);
    match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// The words of the agents of a removal: `no agent`, `1 agent`, `2 agents`.
fn agents_words(removal: &Removal) -> String {
    match removal.agents.len() {
        0 => "no agent".to_owned(),
        1 => "1 agent".to_owned(),
        count => format!("{count} agents"),
    }
}

/// The line of fzf for a removal: the 3 hidden fields, then the row.
fn line(removal: &Removal, now: u64, width: usize) -> String {
    let left = [
        part("", "  "),
        part(ACCENT, "↺ "),
        part("", removal.branch.clone()),
        part(RULE, " — "),
        part(MUTED, agents_words(removal)),
    ];
    let right = [
        part(MUTED, git::folder_name(&removal.main_checkout)),
        part(MUTED, ago(removal.time, now)),
    ];
    format!(
        "{}\t{}\t{}\t{}",
        removal.time,
        removal.path.display(),
        removal.main_checkout.display(),
        style::row(&left, &right, width)
    )
}

/// The removal of a row: its time and its folder.
fn find<'a>(removals: &'a [Removal], time: &str, path: &str) -> Option<&'a Removal> {
    removals
        .iter()
        .find(|removal| removal.time.to_string() == time && removal.path == Path::new(path))
}

/// Let the user choose a removal, or None after `Esc`.
pub fn choose(removals: &[Removal]) -> Result<Option<Removal>, Error> {
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let binary = shell_quote(&executable.to_string_lossy());
    let columns = style::terminal_columns();
    let scope = if removals.len() == 1 {
        "1 removed".to_owned()
    } else {
        format!("{} removed", removals.len())
    };
    let mut child = Command::new("fzf")
        .env(style::COLUMNS_VARIABLE, columns.to_string())
        .args(style::fzf_arguments(GHOST, &scope, &HINTS))
        .arg("--preview")
        .arg(format!("{binary} restorer-preview {{1}} {{2}}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Command(format!("cannot run fzf: {error}")))?;
    if let Some(mut input) = child.stdin.take() {
        let width = style::row_width(columns);
        let now = stale::now();
        let rows: String = removals
            .iter()
            .rev()
            .map(|removal| line(removal, now, width) + "\n")
            .collect();
        // fzf closes its input when the user quits.
        let _ = input.write_all(rows.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| Error::Command(format!("fzf failed: {error}")))?;
    match output.status.code() {
        Some(0) => {}
        Some(FZF_CANCEL_EXIT_CODE) | Some(FZF_NO_MATCH_EXIT_CODE) => return Ok(None),
        _ => return Err(Error::Command(style::FZF_FAILED.to_owned())),
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.lines().next().unwrap_or_default().split('\t');
    let (time, path) = (
        fields.next().unwrap_or_default(),
        fields.next().unwrap_or_default(),
    );
    Ok(find(removals, time, path).cloned())
}

/// fzf event `restorer-preview`: what comes back with the removal of the
/// row.
pub fn print_preview(state_dir: &Path, time: &str, path: &str) {
    let removals = restore::all(state_dir);
    let Some(removal) = find(&removals, time, path) else {
        println!("{MUTED}No record for this worktree.{RESET}");
        return;
    };
    let now = stale::now();
    println!(
        "{BOLD}{ACCENT}Restore{RESET}{RULE} · {RESET}{MUTED}removed {} ago{RESET}",
        ago(removal.time, now)
    );
    println!();
    let label = |name: &str| format!("{BOLD}{name:<10}{RESET}");
    println!("{}{}", label("Branch"), removal.branch);
    println!(
        "{}{REPOSITORY}{}{RESET}",
        label("Project"),
        git::folder_name(&removal.main_checkout)
    );
    println!("{}{}", label("Folder"), home_text(&removal.path));
    let short_commit: String = removal.commit.chars().take(8).collect();
    let branch_state = if git::branch_commit(&removal.main_checkout, &removal.branch).is_some() {
        format!("{GREEN}the branch is there{RESET}")
    } else if git::branch_commit(&removal.main_checkout, &removal.commit).is_some() {
        format!("{YELLOW}the branch comes back on its commit{RESET}")
    } else {
        format!("{YELLOW}the branch is gone{RESET}")
    };
    println!("{}{short_commit}  {branch_state}", label("Commit"));
    if removal.path.exists() {
        println!("{}{YELLOW}already there{RESET}", label("Worktree"));
    }
    println!();
    if removal.agents.is_empty() {
        println!("{DIM}No agent comes back.{RESET}");
    } else {
        println!("{BOLD}Agents{RESET} {DIM}(a new tab each, with its conversation){RESET}");
        for agent in &removal.agents {
            let resume = agent
                .resume_arguments()
                .map(|arguments| format!("{} {}", agent.kind, arguments.join(" ")))
                .unwrap_or_else(|| format!("{}: cannot resume", agent.kind));
            println!("  {GREEN}↺{RESET} {resume}");
        }
    }
    if let Some(command) = &removal.recover_command {
        println!();
        println!("{DIM}If the commit is gone: {command}{RESET}");
    }
    println!();
    println!("{DIM}The programs of the other panes, and the files{RESET}");
    println!("{DIM}that an editor did not save, do not come back.{RESET}");
}

/// A path for the user: `~/…` in the home folder.
fn home_text(path: &Path) -> String {
    match env::var_os("HOME") {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::RemovedAgent;
    use std::path::PathBuf;

    fn removal(branch: &str, agents: usize) -> Removal {
        Removal {
            time: 1000,
            main_checkout: PathBuf::from("/work/app"),
            path: PathBuf::from(format!("/work/app.{branch}")),
            branch: branch.to_owned(),
            commit: "abc123".to_owned(),
            agents: (0..agents)
                .map(|index| RemovedAgent {
                    kind: "claude".to_owned(),
                    session: format!("s{index}"),
                })
                .collect(),
            recover_command: None,
            tabs: Vec::new(),
        }
    }

    #[test]
    fn the_time_since_a_removal_is_short() {
        assert_eq!(ago(1000, 1030), "now");
        assert_eq!(ago(1000, 1000 + 5 * 60), "5m");
        assert_eq!(ago(1000, 1000 + 3 * 3600), "3h");
        assert_eq!(ago(1000, 1000 + 2 * 86_400), "2d");
        assert_eq!(ago(2000, 1000), "now");
    }

    #[test]
    fn a_row_names_its_removal_in_its_hidden_fields() {
        let removals = [removal("feat", 2), removal("fix", 0)];
        let text = line(&removals[0], 1000, 60);
        let fields: Vec<&str> = text.splitn(4, '\t').collect();

        assert_eq!(fields[..3], ["1000", "/work/app.feat", "/work/app"]);
        assert!(fields[3].contains("feat"));
        assert!(fields[3].contains("2 agents"));
        assert_eq!(
            find(&removals, "1000", "/work/app.fix").unwrap().branch,
            "fix"
        );
        assert!(find(&removals, "999", "/work/app.fix").is_none());
        assert_eq!(agents_words(&removals[1]), "no agent");
    }
}
