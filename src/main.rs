//! herdr-worktrunk: manage the git worktrees with worktrunk (wt) in herdr,
//! and keep the rule "1 git worktree = 1 herdr workspace".
//!
//! Usage:
//!   herdr-worktrunk observe                    start the observer in the background
//!   herdr-worktrunk observe --run              the observer itself
//!   herdr-worktrunk open picker|remover|merger an action: open a popup
//!   herdr-worktrunk pick|confirm|merger|message  the command of a popup
//!   herdr-worktrunk prune-merged --dry-run [<folder>]
//!                                              the worktrees of the merged pull requests

mod actions;
mod config;
mod git;
mod github;
mod herdr;
mod observer;
mod picker;
mod process;
mod prune;
mod ui;
mod wt;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use herdr::Error;

const USAGE: &str = "usage: herdr-worktrunk observe | open picker|remover|merger | pick | confirm | merger | message | prune-merged --dry-run [<folder>]";

/// A folder that herdr gives in an environment variable, else a default.
fn herdr_folder(variable: &str, default_parts: &[&str]) -> PathBuf {
    if let Some(folder) = env::var_os(variable) {
        return PathBuf::from(folder);
    }
    let mut folder = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    folder.extend(default_parts);
    folder
}

/// Start the observer (`observe`), or be the observer (`observe --run`).
fn observe(is_observer: bool) -> Result<(), Error> {
    let config_dir = herdr_folder(
        "HERDR_PLUGIN_CONFIG_DIR",
        &[".config", "herdr", "plugins", "config", herdr::PLUGIN_ID],
    );
    let config = config::load(&config_dir);
    if !config.observe {
        return Ok(());
    }
    let state_dir = herdr_folder(
        "HERDR_PLUGIN_STATE_DIR",
        &[".local", "state", "herdr", "plugins", herdr::PLUGIN_ID],
    );
    if is_observer {
        observer::run(&state_dir, config.prune_merged)
    } else {
        observer::start(&state_dir)
    }
}

/// Print the worktrees of the merged pull requests in the repository of
/// `folder`: the worktrees that the observer removes with `prune_merged`.
/// Remove no worktree. As the observer, run `git fetch --prune` first.
/// This command does not know the agents of herdr.
fn prune_dry_run(folder: &Path) -> Result<(), Error> {
    let checkout = git::checkout_of(folder).ok_or_else(|| {
        Error::Command(format!("{} is not in a git repository", folder.display()))
    })?;
    for worktree in prune::merged_worktrees(&checkout.main_checkout(), |_| false)? {
        println!(
            "would remove {} ({})",
            worktree.path.display(),
            worktree.branch
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
        ["prune-merged", "--dry-run", folder @ ..] if folder.len() <= 1 => {
            let folder = Path::new(folder.first().copied().unwrap_or("."));
            if let Err(error) = prune_dry_run(folder) {
                eprintln!("herdr-worktrunk: {error}");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        ["observe"] => (observe(false), None),
        ["observe", "--run"] => (observe(true), None),
        ["open", entrypoint @ ("picker" | "remover" | "merger")] => {
            (actions::open(entrypoint), None)
        }
        ["pick"] => (actions::pick(), Some("pick")),
        ["confirm"] => (actions::confirm(), Some("confirm")),
        ["merger"] => (actions::merger(), Some("merger")),
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
