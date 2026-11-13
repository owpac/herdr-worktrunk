//! herdr-worktrunk: manage the git worktrees with worktrunk (wt) in herdr,
//! and keep the rule "1 git worktree = 1 herdr workspace".
//!
//! Usage:
//!   herdr-worktrunk observe                    start the observer in the background
//!   herdr-worktrunk observe --run              the observer itself
//!   herdr-worktrunk open picker|remover|merger|cleaner|restorer  an action: open a popup
//!   herdr-worktrunk pick|confirm|merger|cleaner|message  the command of a popup
//!   herdr-worktrunk list-merged [<folder>]     list the worktrees of the finished pull requests and of the merged branches
//!   herdr-worktrunk list-stale [<folder>]      list the old worktrees without pull request
//!   herdr-worktrunk clean [<folder>]           open the popup cleaner from a shell

mod actions;
mod azure;
mod cleaner;
mod config;
mod forge;
mod fzf;
mod git;
mod gitea;
mod github;
mod gitlab;
mod herdr;
mod leftover;
mod observer;
mod picker;
mod process;
mod prune;
mod restore;
mod restorer;
mod stale;
mod style;
mod sync;
#[cfg(test)]
mod test_support;
mod theme;
mod ui;
mod url;
mod wt;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use herdr::Error;

const USAGE: &str = "usage: herdr-worktrunk observe [--run | --hand-over] | open picker|remover|merger|cleaner|restorer | pick | confirm | merger | restorer | restore-run | cleaner | message | list-merged [<folder>] | list-stale [<folder>] | clean [<folder>] | remove --path <worktree> [--close-agents]";

/// The days without activity of `list-stale` when `stale_days` is 0.
const DEFAULT_STALE_DAYS: u32 = 7;

/// A folder that herdr gives in an environment variable, else a default.
fn herdr_folder(variable: &str, default_parts: &[&str]) -> PathBuf {
    if let Some(folder) = env::var_os(variable) {
        return PathBuf::from(folder);
    }
    let mut folder = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    folder.extend(default_parts);
    folder
}

/// The state folder of the plugin: the observer writes its files there.
pub(crate) fn state_dir() -> PathBuf {
    herdr_folder(
        "HERDR_PLUGIN_STATE_DIR",
        &[".local", "state", "herdr", "plugins", herdr::PLUGIN_ID],
    )
}

/// The config of the plugin, from the config folder that herdr gives.
fn load_config() -> config::Config {
    config::current()
}

/// The main checkout of the repository of `folder`, or an error for the
/// commands `list-merged` and `list-stale`.
fn main_checkout_of(folder: &Path) -> Result<PathBuf, Error> {
    git::checkout_of(folder)
        .map(|checkout| checkout.main_checkout())
        .ok_or_else(|| Error::Command(format!("{} is not in a git repository", folder.display())))
}

/// What the command `observe` does.
enum Observe {
    /// Start the observer in the background (`observe`).
    Start,
    /// Be the observer (`observe --run`).
    Run,
    /// Give the path of this binary to the running observer, without a
    /// start (`observe --hand-over`).
    HandOver,
}

/// The command `observe`. `observe = false` in config.toml turns it off.
fn observe(what: Observe) -> Result<(), Error> {
    let config = load_config();
    if !config.observe {
        return Ok(());
    }
    let state_dir = state_dir();
    match what {
        Observe::Start => observer::start(&state_dir),
        Observe::Run => observer::run(&state_dir, config.prune_merged, config.stale_days),
        Observe::HandOver => observer::hand_over(&state_dir),
    }
}

/// Print the worktrees of the merged or closed pull requests in the
/// repository of `folder`, then the worktrees whose branch is in the
/// default branch, with what the observer does with them (`prune_merged`).
/// Remove no worktree. This command does not know the agents of herdr: an
/// agent that works keeps any worktree.
fn list_merged(folder: &Path) -> Result<(), Error> {
    let main_checkout = main_checkout_of(folder)?;
    let check = prune::check_worktrees(&main_checkout)?;
    for (branch, error) in &check.errors {
        eprintln!("{branch}: not checked: {error}");
    }
    for worktree in check.finished {
        let state = match worktree.pull_request.state {
            forge::PullRequestState::Closed => "closed",
            _ => "merged",
        };
        println!(
            "{} ({}, {} {state})",
            worktree.path.display(),
            worktree.branch,
            worktree.reference
        );
    }
    for worktree in check.merged {
        let history = leftover::History::of(git::has_own_commits(&main_checkout, &worktree.branch));
        let days =
            git::last_activity(&worktree.path).map(|time| stale::days_since(time, stale::now()));
        let removed = leftover::merged_branch(history, days, leftover::Agents::None)
            == Some(leftover::Reason::BranchMerged);
        let fate = match (git::is_clean(&worktree.path), history, removed) {
            (false, _, _) => "kept: changes not committed",
            (true, leftover::History::OwnCommits, _) => {
                "own commits: removed, idle agents close first"
            }
            (true, _, true) => "no own commit: removed if no agent is in it",
            (true, _, false) => "no own commit: kept, activity less than 1 day ago",
        };
        println!(
            "{} ({}, in {}, {fate})",
            worktree.path.display(),
            worktree.branch,
            worktree.default_branch
        );
    }
    Ok(())
}

/// The days without activity of an old worktree for the cleaning:
/// `stale_days` of the config, or 7 days if the key is 0.
fn stale_days_or_default() -> u32 {
    stale_days_of(&load_config())
}

/// The days of `stale_days`, or `DEFAULT_STALE_DAYS` if it is 0.
fn stale_days_of(config: &config::Config) -> u32 {
    match config.stale_days {
        0 => DEFAULT_STALE_DAYS,
        days => days,
    }
}

/// Print the old worktrees without pull request in the repository of
/// `folder`: the worktrees that the observer names (`stale_days` of the
/// config, or 7 days). This command does not know the agents of herdr, and
/// ignores the last questions.
fn list_stale(folder: &Path) -> Result<(), Error> {
    let main_checkout = main_checkout_of(folder)?;
    let stale_days = stale_days_or_default();
    let worktrees = git::linked_worktrees(&main_checkout)?;
    let old = stale::select_old(&main_checkout, worktrees, |_| false, None, stale_days);
    for worktree in prune::without_pull_request(&main_checkout, old)? {
        println!(
            "{} ({}, no change for {} days)",
            worktree.path.display(),
            worktree.branch,
            worktree.days
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    // `popup` is the command of a popup: its errors show in a message box.
    let (result, popup) = match arguments.as_slice() {
        ["message"] => {
            actions::message();
            return ExitCode::SUCCESS;
        }
        // The events of the fzf picker.
        ["picker-rows", repository, cache, query @ ..] => {
            let query = query.first().copied().unwrap_or_default();
            picker::print_rows(Path::new(repository), Path::new(cache), query);
            return ExitCode::SUCCESS;
        }
        ["picker-preview", repository, cache, kind, branch, path] => {
            picker::print_preview(Path::new(repository), Path::new(cache), kind, branch, path);
            return ExitCode::SUCCESS;
        }
        ["cleaner-rows", state, query @ ..] => {
            cleaner::print_rows(Path::new(state), &query.join(" "));
            return ExitCode::SUCCESS;
        }
        ["cleaner-move", state, direction, current_index, current_id, query @ ..] => {
            let direction = match *direction {
                "up" => cleaner::Move::Up,
                "down" => cleaner::Move::Down,
                _ => cleaner::Move::Keep,
            };
            cleaner::print_move(
                Path::new(state),
                direction,
                current_index,
                current_id,
                &query.join(" "),
            );
            return ExitCode::SUCCESS;
        }
        ["cleaner-remember", state, id] => {
            cleaner::remember_cursor(Path::new(state), id);
            return ExitCode::SUCCESS;
        }
        ["cleaner-enter", kind] => {
            cleaner::print_enter(kind);
            return ExitCode::SUCCESS;
        }
        ["cleaner-delete-all", state, visible] => {
            cleaner::print_delete_all(Path::new(state), Path::new(visible));
            return ExitCode::SUCCESS;
        }
        ["restorer-preview", time, path] => {
            restorer::print_preview(&state_dir(), time, path);
            return ExitCode::SUCCESS;
        }
        ["cleaner-preview", state, stale_days, id, branch, main_checkout, kind] => {
            let stale_days = stale_days.parse().unwrap_or(DEFAULT_STALE_DAYS);
            cleaner::print_preview(
                Path::new(state),
                stale_days,
                id,
                branch,
                main_checkout,
                kind,
            );
            return ExitCode::SUCCESS;
        }
        ["picker-tab", cache, direction, count @ ..] => {
            // The picker has 3 tabs, the cleaner 4.
            let count = count
                .first()
                .and_then(|count| count.parse().ok())
                .unwrap_or(3);
            picker::change_tab(Path::new(cache), *direction == "next", count);
            return ExitCode::SUCCESS;
        }
        ["picker-backspace", query, kind, branch] => {
            picker::backspace_action(query, kind, branch);
            return ExitCode::SUCCESS;
        }
        // The detached process that opens the next popup.
        ["open-later", entrypoint, folder, assignments @ ..] => {
            actions::open_later(entrypoint, Path::new(folder), assignments);
            return ExitCode::SUCCESS;
        }
        [command @ ("list-merged" | "list-stale"), folder @ ..] if folder.len() <= 1 => {
            let folder = Path::new(folder.first().copied().unwrap_or("."));
            let result = if *command == "list-merged" {
                list_merged(folder)
            } else {
                list_stale(folder)
            };
            if let Err(error) = result {
                eprintln!("herdr-worktrunk: {error}");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        ["clean", folder @ ..] if folder.len() <= 1 => {
            let scope = folder.first().map(Path::new);
            if let Err(error) = actions::open_cleaner_from_shell(scope) {
                eprintln!("herdr-worktrunk: {error}");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        // A removal for another plugin: no popup, no notification. The
        // caller shows the message.
        ["remove", arguments @ ..] => {
            let Some((path, close_agents)) = actions::parse_remove_arguments(arguments) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            return match actions::remove_from_command(&path, close_agents) {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("herdr-worktrunk: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        ["observe"] => (observe(Observe::Start), None),
        ["observe", "--run"] => (observe(Observe::Run), None),
        ["observe", "--hand-over"] => (observe(Observe::HandOver), None),
        ["open", entrypoint @ ("picker" | "remover" | "merger" | "cleaner" | "restorer")] => {
            // herdr runs `observe` only at its start and for a new agent, not after an update
            // or a link. Each action gives this binary to the observer, and starts the observer
            // if it stopped. A linked checkout is the active version: its binary is the right
            // one. An error stays without effect on the action.
            if let Err(error) = observe(Observe::Start) {
                eprintln!("herdr-worktrunk: {error}");
            }
            (actions::open(entrypoint), None)
        }
        ["pick"] => (actions::pick(), Some("pick")),
        ["confirm"] => (actions::confirm(), Some("confirm")),
        ["merger"] => (actions::merger(), Some("merger")),
        ["restorer"] => (actions::restorer(), Some("restorer")),
        ["restore-run"] => (actions::restore_run(), Some("restore-run")),
        ["cleaner"] => (
            {
                let config = load_config();
                actions::cleaner(stale_days_of(&config), config.fetch_before_check)
            },
            Some("cleaner"),
        ),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            match popup {
                Some(command) => {
                    ui::show_message(actions::popup_title(command), &error.to_string());
                }
                None => {
                    eprintln!("herdr-worktrunk: {error}");
                    herdr::notify(&error.to_string());
                }
            }
            ExitCode::FAILURE
        }
    }
}
