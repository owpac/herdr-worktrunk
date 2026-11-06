//! The config file of the plugin: `config.toml` in the config folder that
//! herdr gives to the plugin (`herdr plugin config-dir trunk`).
//!
//! The agents also read this file (the skill worktrees of the agent
//! plugin herdr): `agent_mode` and the `[repositories."<path>"]` tables
//! are for them. The plugin reads only the top-level key `observe`.

use std::fs;
use std::path::Path;

/// The options of the plugin.
#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    /// Start the observer (default: true).
    pub observe: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { observe: true }
    }
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
            // The tables are for the agents.
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.split('#').next().unwrap_or_default().trim();
        if key.trim() == "observe" {
            config.observe = value != "false";
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_starts_the_observer() {
        assert_eq!(parse(""), Config { observe: true });
    }

    #[test]
    fn observe_false_stops_the_observer() {
        let text = "agent_mode = \"ask\"\nobserve = false # no observer\n";

        assert_eq!(parse(text), Config { observe: false });
    }

    #[test]
    fn a_key_in_a_table_is_not_an_option_of_the_plugin() {
        let text = "[repositories.\"~/.dotfiles\"]\nobserve = false\n";

        assert_eq!(parse(text), Config { observe: true });
    }
}
