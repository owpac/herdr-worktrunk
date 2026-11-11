//! The config file of the plugin: `config.toml` in the config folder that
//! herdr gives to the plugin (`herdr plugin config-dir herdr-worktrunk`).
//!
//! The plugin reads only the top-level keys `observe`, `prune_merged`,
//! `stale_days`, `popup_size` and `fetch_before_check`, and ignores the
//! other keys and the tables.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::herdr::PLUGIN_ID;

/// The size of the large popups: the picker, the cleaner and the merger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupSize {
    /// The size of the popup of herdr-bar: 74 % × 62 % of the window.
    #[default]
    Default,
    /// 90 % × 80 % of the window.
    Big,
}

/// The popups that have 2 sizes: the manifest gives each one a second
/// pane, `<name>-big`.
const SIZED_POPUPS: [&str; 4] = ["picker", "cleaner", "merger", "restorer"];

impl PopupSize {
    /// The pane of the manifest for the popup `entrypoint` in this size.
    pub fn entrypoint(self, entrypoint: &str) -> String {
        match self {
            PopupSize::Big if SIZED_POPUPS.contains(&entrypoint) => format!("{entrypoint}-big"),
            _ => entrypoint.to_owned(),
        }
    }
}

/// The options of the plugin.
#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    /// Start the observer (default: true).
    pub observe: bool,
    /// The observer removes the worktrees of the merged or closed pull
    /// requests (default: false).
    pub prune_merged: bool,
    /// The observer names in a notification the worktrees without pull
    /// request and without change for this number of days (default: 0,
    /// off). It needs `prune_merged`.
    pub stale_days: u32,
    /// The size of the large popups (default: the size of herdr-bar).
    pub popup_size: PopupSize,
    /// The popup cleaner fetches `origin` before its checks (default:
    /// false). The fetch can ask the ssh agent for a key.
    pub fetch_before_check: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            observe: true,
            prune_merged: false,
            stale_days: 0,
            popup_size: PopupSize::Default,
            fetch_before_check: false,
        }
    }
}

/// The config folder of the plugin: the folder that herdr gives, else the
/// default folder of herdr.
pub fn config_dir() -> PathBuf {
    if let Some(folder) = env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(folder);
    }
    let mut folder = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    folder.extend([".config", "herdr", "plugins", "config", PLUGIN_ID]);
    folder
}

/// The config of the plugin, from its config folder.
pub fn current() -> Config {
    load(&config_dir())
}

/// Read the config of the plugin. A missing file gives the defaults.
pub fn load(config_dir: &Path) -> Config {
    match fs::read_to_string(config_dir.join("config.toml")) {
        Ok(text) => parse(&text),
        Err(_) => Config::default(),
    }
}

/// Read the top-level keys that the plugin knows, and ignore the others.
fn parse(text: &str) -> Config {
    let mut config = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // The keys of a table are not top-level keys.
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.split('#').next().unwrap_or_default().trim();
        match key.trim() {
            "observe" => config.observe = value != "false",
            "prune_merged" => config.prune_merged = value == "true",
            "stale_days" => config.stale_days = value.parse().unwrap_or(0),
            "fetch_before_check" => config.fetch_before_check = value == "true",
            "popup_size" => {
                config.popup_size = match value.trim_matches('"') {
                    "big" => PopupSize::Big,
                    _ => PopupSize::Default,
                }
            }
            _ => {}
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_gives_the_defaults() {
        assert_eq!(parse(""), Config::default());
        assert!(Config::default().observe);
        assert!(!Config::default().prune_merged);
    }

    #[test]
    fn observe_false_stops_the_observer() {
        let text = "other = \"x\"\nobserve = false # no observer\n";

        assert!(!parse(text).observe);
    }

    #[test]
    fn prune_merged_true_removes_the_merged_worktrees() {
        let text = "prune_merged = true # with gh\n";

        assert!(parse(text).prune_merged);
    }

    #[test]
    fn stale_days_gives_a_number_of_days() {
        assert_eq!(parse("stale_days = 7 # a week\n").stale_days, 7);
        assert_eq!(parse("stale_days = \"x\"\n").stale_days, 0);
        assert_eq!(Config::default().stale_days, 0);
    }

    #[test]
    fn fetch_before_check_is_off_by_default() {
        assert!(!Config::default().fetch_before_check);
        assert!(parse("fetch_before_check = true # fetch origin\n").fetch_before_check);
        assert!(!parse("fetch_before_check = 1\n").fetch_before_check);
    }

    #[test]
    fn popup_size_big_gives_the_big_panes() {
        let config = parse("popup_size = \"big\" # 90 % x 80 %\n");

        assert_eq!(config.popup_size, PopupSize::Big);
        assert_eq!(config.popup_size.entrypoint("picker"), "picker-big");
        assert_eq!(config.popup_size.entrypoint("confirm"), "confirm");
        assert_eq!(PopupSize::Default.entrypoint("cleaner"), "cleaner");
        assert_eq!(
            parse("popup_size = \"huge\"\n").popup_size,
            PopupSize::Default
        );
    }

    #[test]
    fn a_key_in_a_table_is_not_an_option_of_the_plugin() {
        let text = "[repositories.\"~/.dotfiles\"]\nobserve = false\nprune_merged = true\n";

        assert_eq!(parse(text), Config::default());
    }
}
