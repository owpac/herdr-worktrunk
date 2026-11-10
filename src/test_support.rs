//! Helpers for the tests: temporary folders and git without the config of
//! the user.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A new empty folder for one test, in the temporary folder.
pub fn test_folder(name: &str) -> PathBuf {
    let folder = env::temp_dir().join(format!(
        "herdr-worktrunk-test-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&folder);
    fs::create_dir_all(&folder).unwrap();
    folder
}

/// Run git in `folder` for a test, without the global config (for
/// example a signature with an agent).
pub fn git_in(folder: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?}");
}

/// The output of git in `folder`, without the final new line.
pub fn git_output(folder: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
