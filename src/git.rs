//! The git facts that the plugin needs about a folder.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A git checkout: the main checkout or a linked worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The root folder of the checkout.
    pub root: PathBuf,
    /// The git folder that all the checkouts of the repository share.
    /// Two checkouts with the same common folder are in the same
    /// repository.
    pub common_dir: PathBuf,
}

impl Checkout {
    /// The main checkout of the repository: the parent of the common
    /// git folder.
    pub fn main_checkout(&self) -> PathBuf {
        match self.common_dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => self.root.clone(),
        }
    }
}

/// Return the checkout that holds `folder`, or None if `folder` is not in
/// a git checkout (or does not exist).
pub fn checkout_of(folder: &Path) -> Option<Checkout> {
    let output = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let root = PathBuf::from(lines.next()?);
    let common_dir = PathBuf::from(lines.next()?);
    Some(Checkout { root, common_dir })
}

/// Return the branch of the checkout `root`, or None for a detached HEAD.
pub fn branch_of(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_main_checkout_is_the_parent_of_the_common_folder() {
        let checkout = Checkout {
            root: PathBuf::from("/work/repo.feature"),
            common_dir: PathBuf::from("/work/repo/.git"),
        };

        assert_eq!(checkout.main_checkout(), PathBuf::from("/work/repo"));
    }

    #[test]
    fn a_folder_outside_git_has_no_checkout() {
        assert_eq!(checkout_of(Path::new("/")), None);
    }
}
