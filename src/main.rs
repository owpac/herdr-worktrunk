//! herdr-worktrunk: manage the git worktrees with worktrunk (wt) in herdr,
//! and keep the rule "1 git worktree = 1 herdr workspace".
//!
//! Usage:
//!   herdr-worktrunk observe                    start the observer in the background
//!   herdr-worktrunk observe --run              the observer itself
//!   herdr-worktrunk open picker|remover|merger an action: open a popup
//!   herdr-worktrunk pick|remove|merge          the command of a popup

mod actions;
mod config;
mod git;
mod herdr;
mod observer;
mod wt;

use std::env;
use std::io::{self, BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use herdr::Error;

const USAGE: &str =
    "usage: herdr-worktrunk observe | open picker|remover|merger | pick | remove | merge";

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

/// In a popup, wait for Enter: the message then stays on the screen.
fn wait_for_enter() {
    if io::stdin().is_terminal() {
        println!("Press Enter to close.");
        let mut line = String::new();
        // The pane closes after this read: its result does not matter.
        let _ = io::stdin().lock().read_line(&mut line);
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (result, is_popup) = match arguments.as_slice() {
        ["observe"] => (observe(false), false),
        ["observe", "--run"] => (observe(true), false),
        ["open", entrypoint @ ("picker" | "remover" | "merger")] => {
            (actions::open(entrypoint), false)
        }
        ["pick"] => (actions::pick(), true),
        ["remove"] => (actions::remove(), true),
        ["merge"] => (actions::merge(), true),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-worktrunk: {error}");
            if is_popup {
                wait_for_enter();
            } else {
                herdr::notify(&error.to_string());
            }
            ExitCode::FAILURE
        }
    }
}
