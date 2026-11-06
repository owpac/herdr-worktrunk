//! herdr-worktrunk: manage the git worktrees with worktrunk (wt) in herdr,
//! and keep the rule "1 git worktree = 1 herdr workspace".
//!
//! Usage:
//!   herdr-worktrunk observe                    start the observer in the background
//!   herdr-worktrunk observe --run              the observer itself
//!   herdr-worktrunk open picker|remover|merger an action: open a popup
//!   herdr-worktrunk pick|remove|merge|message  the command of a popup

mod actions;
mod config;
mod git;
mod herdr;
mod observer;
mod picker;
mod ui;
mod wt;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use herdr::Error;

const USAGE: &str =
    "usage: herdr-worktrunk observe | open picker|remover|merger | pick | remove | merge | message";

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
    if !config::load(&config_dir).observe {
        return Ok(());
    }
    let state_dir = herdr_folder(
        "HERDR_PLUGIN_STATE_DIR",
        &[".local", "state", "herdr", "plugins", herdr::PLUGIN_ID],
    );
    if is_observer {
        observer::run(&state_dir)
    } else {
        observer::start(&state_dir)
    }
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
        ["picker-rows", cache, query @ ..] => {
            picker::print_rows(Path::new(cache), query.first().copied().unwrap_or_default());
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
        ["picker-backspace", repository, cache, query @ ..] => {
            let query = query.first().copied().unwrap_or_default();
            picker::backspace_action(Path::new(repository), Path::new(cache), query);
            return ExitCode::SUCCESS;
        }
        ["picker-remove", repository, cache, kind, branch] => {
            picker::remove(Path::new(repository), Path::new(cache), kind, branch);
            return ExitCode::SUCCESS;
        }
        ["observe"] => (observe(false), None),
        ["observe", "--run"] => (observe(true), None),
        ["open", entrypoint @ ("picker" | "remover" | "merger")] => {
            (actions::open(entrypoint), None)
        }
        ["pick"] => (actions::pick(), Some("pick")),
        ["remove"] => (actions::remove(), Some("remove")),
        ["merge"] => (actions::merge(), Some("merge")),
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
