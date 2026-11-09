//! herdr-worktrunk: manage the git worktrees with worktrunk (wt) in herdr,
//! and keep the rule "1 git worktree = 1 herdr workspace".
//!
//! Usage:
//!   herdr-worktrunk observe                    start the observer in the background
//!   herdr-worktrunk observe --run              the observer itself
//!   herdr-worktrunk open picker|remover|merger|cleaner  an action: open a popup
//!   herdr-worktrunk pick|confirm|merger|cleaner|message  the command of a popup
//!   herdr-worktrunk list-merged [<folder>]     list the worktrees of the merged or closed pull requests
//!   herdr-worktrunk list-stale [<folder>]      list the old worktrees without pull request
//!   herdr-worktrunk clean [<folder>]           open the popup cleaner from a shell

mod actions;
mod cleaner;
mod config;
mod fzf;
mod git;
mod github;
mod herdr;
mod observer;
mod picker;
mod process;
mod prune;
mod stale;
mod style;
mod ui;
mod wt;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use herdr::Error;

const USAGE: &str = "usage: herdr-worktrunk observe | open picker|remover|merger|cleaner | pick | confirm | merger | cleaner | message | list-merged [<folder>] | list-stale [<folder>] | clean [<folder>]";

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
fn state_dir() -> PathBuf {
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

/// Start the observer (`observe`), or be the observer (`observe --run`).
fn observe(is_observer: bool) -> Result<(), Error> {
    let config = load_config();
    if !config.observe {
        return Ok(());
    }
    let state_dir = state_dir();
    if is_observer {
        observer::run(&state_dir, config.prune_merged, config.stale_days)
    } else {
        observer::start(&state_dir)
    }
}

/// Print the worktrees of the merged or closed pull requests in the
/// repository of `folder`: the worktrees that the observer removes with
/// `prune_merged`.
/// Remove no worktree. This command does not know the agents of herdr.
fn list_merged(folder: &Path) -> Result<(), Error> {
    for worktree in prune::check_worktrees(&main_checkout_of(folder)?, |_| false)?.finished {
        let state = match worktree.pull_request.state {
            github::PullRequestState::Closed => "closed",
            _ => "merged",
        };
        println!(
            "{} ({}, pull request #{} {state})",
            worktree.path.display(),
            worktree.branch,
            worktree.pull_request.number
        );
    }
    Ok(())
}

/// The days without activity of an old worktree for the cleaning:
/// `stale_days` of the config, or 7 days if the key is 0.
fn stale_days_or_default() -> u32 {
    match load_config().stale_days {
        0 => DEFAULT_STALE_DAYS,
        days => days,
    }
}

/// Print the old worktrees without pull request in the repository of
/// `folder`: the worktrees of the daily question (`stale_days` of the
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
        ["cleaner-move", state, direction, current, query @ ..] => {
            let direction = match *direction {
                "up" => cleaner::Move::Up,
                "down" => cleaner::Move::Down,
                _ => cleaner::Move::Keep,
            };
            cleaner::print_move(Path::new(state), direction, current, &query.join(" "));
            return ExitCode::SUCCESS;
        }
        ["cleaner-enter", kind] => {
            cleaner::print_enter(kind);
            return ExitCode::SUCCESS;
        }
        ["cleaner-delete-all", state] => {
            cleaner::print_delete_all(Path::new(state));
            return ExitCode::SUCCESS;
        }
        ["cleaner-preview", state, id, branch, main_checkout, kind] => {
            cleaner::print_preview(Path::new(state), id, branch, main_checkout, kind);
            return ExitCode::SUCCESS;
        }
        ["picker-tab", cache, direction] => {
            picker::change_tab(Path::new(cache), *direction == "next");
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
        ["observe"] => (observe(false), None),
        ["observe", "--run"] => (observe(true), None),
        ["open", entrypoint @ ("picker" | "remover" | "merger" | "cleaner")] => {
            (actions::open(entrypoint), None)
        }
        ["pick"] => (actions::pick(), Some("pick")),
        ["confirm"] => (actions::confirm(), Some("confirm")),
        ["merger"] => (actions::merger(), Some("merger")),
        ["cleaner"] => (actions::cleaner(stale_days_or_default()), Some("cleaner")),
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
