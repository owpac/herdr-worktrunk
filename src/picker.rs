//! The worktree picker of the plugin: fzf over the worktrees and the
//! branches that `wt list` gives.
//!
//! Keys (the header of the picker shows them):
//!
//! - `Enter` opens the selected worktree or branch. When the typed name
//!   matches no branch, the first row is `+ Create worktree <name>`:
//!   `Enter` on this row creates the worktree.
//! - `⌫` removes the selected worktree with `wt remove`, after a question,
//!   when the filter is empty. When the filter has text, `⌫` erases a
//!   character, as usual.
//! - `Tab` and `shift-Tab` change the tab of the preview: the diff since
//!   the branch left the default branch, the log, and the changes that
//!   are not committed. `shift-↑` and `shift-↓` scroll the preview.
//! - `Esc` cancels.
//!
//! fzf runs this binary for its events: `picker-rows` gives the rows for
//! the typed name, `picker-preview` gives the preview, `picker-tab`
//! changes the tab of the preview, `picker-backspace` decides what `⌫`
//! does, and `picker-remove` removes a worktree.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::git;
use crate::herdr::Error;
use crate::ui;

/// fzf exits with 130 when the user cancels.
const FZF_CANCEL_EXIT_CODE: i32 = 130;
/// fzf exits with 1 when no row matches.
const FZF_NO_MATCH_EXIT_CODE: i32 = 1;
const HEADER: &str =
    "Enter: open or create · ⌫ (empty filter): remove · Tab: preview · Esc: cancel";
const PREVIEW_COMMIT_COUNT: &str = "20";
/// The tabs of the preview, in the order of `Tab`.
const PREVIEW_TABS: [&str; 3] = ["diff", "log", "changes"];

/// A row of the picker. Its fields, separated by tabs: the kind, the
/// branch, the path of the worktree (empty for a branch), then the text
/// that the user sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A worktree of the repository, the main checkout too.
    Worktree {
        branch: String,
        path: PathBuf,
        is_main: bool,
    },
    /// A local branch without a worktree.
    Branch { branch: String },
    /// A new worktree for the typed name.
    Create { name: String },
}

impl Row {
    fn kind(&self) -> &'static str {
        match self {
            Row::Worktree { is_main: true, .. } => "main",
            Row::Worktree { .. } => "worktree",
            Row::Branch { .. } => "branch",
            Row::Create { .. } => "create",
        }
    }

    fn branch(&self) -> &str {
        match self {
            Row::Worktree { branch, .. } | Row::Branch { branch } => branch,
            Row::Create { name } => name,
        }
    }

    /// The line of fzf: kind, branch, path and text, separated by tabs.
    fn line(&self, status: &str) -> String {
        let (path, text) = match self {
            Row::Create { name } => (
                String::new(),
                format!("\x1b[32m+ Create worktree\x1b[0m {name}"),
            ),
            Row::Worktree { path, .. } => {
                // The folder name is sufficient: the full path is long.
                let folder = path.file_name().unwrap_or(path.as_os_str());
                (
                    path.display().to_string(),
                    format!("{status}  \x1b[2m{}\x1b[0m", folder.to_string_lossy()),
                )
            }
            Row::Branch { .. } => (
                String::new(),
                format!("{status}  \x1b[2mbranch, no worktree\x1b[0m"),
            ),
        };
        format!("{}\t{}\t{path}\t{text}", self.kind(), self.branch())
    }
}

/// Read the rows of `wt list --format json --branches`, as fzf lines.
fn list_lines(repository: &Path) -> Result<Vec<String>, Error> {
    let output = Command::new("wt")
        .arg("-C")
        .arg(repository)
        .args(["list", "--format", "json", "--branches"])
        .stderr(Stdio::null())
        .output()
        .map_err(|error| Error::Command(format!("cannot run wt: {error}")))?;
    if !output.status.success() {
        return Err(Error::Command("wt list failed.".to_owned()));
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::Command(format!("wt list: invalid JSON: {error}")))?;
    let items = json["items"].as_array().cloned().unwrap_or_default();
    let mut lines = Vec::new();
    for item in items {
        let Some(branch) = item["branch"].as_str() else {
            continue;
        };
        let status = item["display"]["statusline"].as_str().unwrap_or(branch);
        let row = match item["worktree"]["path"].as_str() {
            Some(path) => Row::Worktree {
                branch: branch.to_owned(),
                path: PathBuf::from(path),
                is_main: item["worktree"]["main"].as_bool().unwrap_or(false),
            },
            None => Row::Branch {
                branch: branch.to_owned(),
            },
        };
        lines.push(row.line(status));
    }
    Ok(lines)
}

/// The rows for the typed name: the create row first (if the name is new),
/// then the rows of `wt list`.
pub fn rows_for_query(cached_lines: &[String], query: &str) -> Vec<String> {
    // A tab separates the fields of a row: a name cannot contain it.
    let name = query.trim().replace('\t', " ");
    let is_known_branch = cached_lines
        .iter()
        .any(|line| line.split('\t').nth(1) == Some(name.as_str()));
    let mut rows = Vec::new();
    if !name.is_empty() && !is_known_branch {
        rows.push(Row::Create { name }.line(""));
    }
    rows.extend(cached_lines.iter().cloned());
    rows
}

/// Put `text` in single quotes for the shell that runs the fzf commands.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The cache of the rows of `wt list`, for the events of fzf.
fn cache_path() -> PathBuf {
    env::temp_dir().join(format!("herdr-worktrunk-picker-{}.tsv", std::process::id()))
}

fn write_cache(cache: &Path, repository: &Path) -> Result<Vec<String>, Error> {
    let lines = list_lines(repository)?;
    fs::write(cache, lines.join("\n"))
        .map_err(|error| Error::Command(format!("cannot write {}: {error}", cache.display())))?;
    Ok(lines)
}

fn read_cache(cache: &Path) -> Vec<String> {
    fs::read_to_string(cache)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// fzf event `picker-rows`: print the rows for the typed name.
pub fn print_rows(cache: &Path, query: &str) {
    let mut stdout = io::stdout().lock();
    for row in rows_for_query(&read_cache(cache), query) {
        // fzf closes the pipe when the user types again: stop without error.
        if writeln!(stdout, "{row}").is_err() {
            return;
        }
    }
}

/// The file that keeps the tab of the preview, next to the cache.
fn tab_path(cache: &Path) -> PathBuf {
    cache.with_extension("tab")
}

fn read_tab(cache: &Path) -> usize {
    fs::read_to_string(tab_path(cache))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|tab| *tab < PREVIEW_TABS.len())
        .unwrap_or(0)
}

/// The label of the preview: the tabs, with the current tab in brackets.
fn preview_label(current: usize) -> String {
    let tabs: Vec<String> = PREVIEW_TABS
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            if index == current {
                format!("[{tab}]")
            } else {
                format!(" {tab} ")
            }
        })
        .collect();
    format!(" {} ", tabs.join(" "))
}

/// fzf event `picker-tab`: go to the next (or the previous) tab of the
/// preview, and print the fzf actions that show it.
pub fn change_tab(cache: &Path, is_next: bool) {
    let count = PREVIEW_TABS.len();
    let current = read_tab(cache);
    let tab = if is_next {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    };
    // Without the file, the preview stays on its first tab.
    let _ = fs::write(tab_path(cache), tab.to_string());
    println!(
        "refresh-preview+change-preview-label:{}",
        preview_label(tab)
    );
}

/// Run a command for the preview, and print its output and its errors.
fn print_command(command: &mut Command) {
    let output = command.env("GIT_PAGER", "cat").env("PAGER", "cat").output();
    match output {
        Ok(output) => {
            print!("{}", String::from_utf8_lossy(&output.stdout));
            print!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Err(error) => println!("cannot run the preview: {error}"),
    }
}

/// fzf event `picker-preview`: print the current tab of the preview.
pub fn print_preview(repository: &Path, cache: &Path, kind: &str, branch: &str, path: &str) {
    if kind == "create" {
        println!("Enter creates the worktree {branch} from the default branch.");
        return;
    }
    match PREVIEW_TABS[read_tab(cache)] {
        "diff" => print_command(Command::new("wt").arg("-C").arg(repository).args([
            "step",
            "diff",
            "--branch",
            branch,
            "--",
            "--color=always",
        ])),
        "log" => print_command(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["log", "--oneline", "--decorate", "--color=always", "-n"])
                .arg(PREVIEW_COMMIT_COUNT)
                .arg(branch)
                .arg("--"),
        ),
        _ if path.is_empty() => println!("{branch} has no worktree: no changes to show."),
        _ => {
            print_command(Command::new("git").arg("-C").arg(path).args([
                "-c",
                "color.status=always",
                "status",
                "--short",
                "--branch",
            ]));
            println!();
            print_command(Command::new("git").arg("-C").arg(path).args([
                "diff",
                "--color=always",
                "HEAD",
            ]));
        }
    }
}

/// The fzf action that removes the selected worktree, then reloads the rows.
fn remove_action(binary: &str, repository: &str, cache: &str) -> String {
    let rows_command = format!("{binary} picker-rows {cache} {{q}}");
    format!(
        "execute({binary} picker-remove {repository} {cache} {{1}} {{2}})+reload({rows_command})"
    )
}

/// fzf event `picker-backspace`: with an empty filter, `⌫` removes the
/// selected worktree. Else it erases a character.
pub fn backspace_action(repository: &Path, cache: &Path, query: &str) {
    if !query.is_empty() {
        println!("backward-delete-char");
        return;
    }
    let Ok(executable) = env::current_exe() else {
        println!("backward-delete-char");
        return;
    };
    println!(
        "{}",
        remove_action(
            &shell_quote(&executable.to_string_lossy()),
            &shell_quote(&repository.to_string_lossy()),
            &shell_quote(&cache.to_string_lossy()),
        )
    );
}

/// fzf event `picker-remove`: remove the selected worktree with wt, after
/// a question, then refresh the cache.
pub fn remove(repository: &Path, cache: &Path, kind: &str, branch: &str) {
    let title = "Worktrunk: remove";
    match kind {
        "worktree" => {}
        "main" => {
            ui::show_message(title, "The main checkout cannot be removed.");
            return;
        }
        _ => {
            ui::show_message(title, &format!("{branch} has no worktree to remove."));
            return;
        }
    }
    let question =
        format!("Remove the worktree of {branch}?\nwt keeps the branch if it is not merged.");
    if !ui::confirm(title, &question, "Remove") {
        return;
    }
    let output = Command::new("wt")
        .arg("-C")
        .arg(repository)
        .args(["remove", "--foreground", branch])
        .stdin(Stdio::inherit())
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let details = String::from_utf8_lossy(&output.stderr);
            ui::show_message(
                title,
                &format!("wt did not remove the worktree.\n{}", details.trim()),
            );
        }
        Err(error) => ui::show_message(title, &format!("cannot run wt: {error}")),
    }
    // The observer closes the workspace of the removed worktree.
    let _ = write_cache(cache, repository);
}

/// Show the picker for `repository`, and return the selected row.
///
/// Returns None if the user cancels.
pub fn pick(repository: &Path) -> Result<Option<Row>, Error> {
    let main_checkout = git::checkout_of(repository)
        .map(|checkout| checkout.main_checkout())
        .unwrap_or_else(|| repository.to_path_buf());
    let cache = cache_path();
    let lines = write_cache(&cache, &main_checkout)?;
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let binary = shell_quote(&executable.to_string_lossy());
    let repository_text = shell_quote(&main_checkout.to_string_lossy());
    let cache_text = shell_quote(&cache.to_string_lossy());
    let rows_command = format!("{binary} picker-rows {cache_text} {{q}}");
    let _ = fs::remove_file(tab_path(&cache));
    let mut fzf = Command::new("fzf")
        .args([
            "--ansi",
            "--layout",
            "reverse",
            "--no-sort",
            "--delimiter",
            "\t",
            "--with-nth",
            "4..",
            "--prompt",
            "worktree> ",
            "--header",
            HEADER,
            "--preview-window",
            "right,50%",
        ])
        .arg("--preview-label")
        .arg(preview_label(0))
        .arg("--preview")
        .arg(format!(
            "{binary} picker-preview {repository_text} {cache_text} {{1}} {{2}} {{3}}"
        ))
        .arg("--bind")
        .arg(format!("change:reload({rows_command})"))
        .arg("--bind")
        .arg(format!(
            "bspace:transform({binary} picker-backspace {repository_text} {cache_text} {{q}})"
        ))
        .arg("--bind")
        .arg(format!(
            "tab:transform({binary} picker-tab {cache_text} next)"
        ))
        .arg("--bind")
        .arg(format!(
            "btab:transform({binary} picker-tab {cache_text} previous)"
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Command(format!("cannot run fzf: {error}")))?;
    if let Some(mut input) = fzf.stdin.take() {
        // fzf can stop before it reads all the rows: ignore a broken pipe.
        let _ = input.write_all(lines.join("\n").as_bytes());
    }
    let output = fzf
        .wait_with_output()
        .map_err(|error| Error::Command(format!("fzf failed: {error}")))?;
    let _ = fs::remove_file(&cache);
    let _ = fs::remove_file(tab_path(&cache));
    match output.status.code() {
        Some(0) => {}
        Some(FZF_CANCEL_EXIT_CODE) | Some(FZF_NO_MATCH_EXIT_CODE) => return Ok(None),
        _ => return Err(Error::Command("fzf failed.".to_owned())),
    }
    let selected = String::from_utf8_lossy(&output.stdout);
    Ok(parse_row(selected.trim_end_matches('\n')))
}

/// Read the row that fzf prints.
fn parse_row(line: &str) -> Option<Row> {
    let mut fields = line.splitn(4, '\t');
    let kind = fields.next()?;
    let branch = fields.next()?.to_owned();
    let path = PathBuf::from(fields.next()?);
    match kind {
        "create" => Some(Row::Create { name: branch }),
        "branch" => Some(Row::Branch { branch }),
        "main" | "worktree" => Some(Row::Worktree {
            branch,
            path,
            is_main: kind == "main",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree_line() -> String {
        Row::Worktree {
            branch: "feat-a".to_owned(),
            path: PathBuf::from("/work/repo.feat-a"),
            is_main: false,
        }
        .line("feat-a  \x1b[2m_\x1b[22m")
    }

    #[test]
    fn a_new_name_gets_the_create_row_first() {
        let rows = rows_for_query(&[worktree_line()], "fix-parser");

        assert!(rows[0].starts_with("create\tfix-parser\t"));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_known_branch_gets_no_create_row() {
        let rows = rows_for_query(&[worktree_line()], "feat-a");

        assert_eq!(rows, [worktree_line()]);
    }

    #[test]
    fn an_empty_query_gets_no_create_row() {
        let rows = rows_for_query(&[worktree_line()], "  ");

        assert_eq!(rows, [worktree_line()]);
    }

    #[test]
    fn a_tab_in_the_name_does_not_break_the_row() {
        let rows = rows_for_query(&[], "a\tb");

        assert!(rows[0].starts_with("create\ta b\t"));
    }

    #[test]
    fn parse_reads_a_worktree_row_and_its_path() {
        let row = parse_row(&worktree_line());

        assert_eq!(
            row,
            Some(Row::Worktree {
                branch: "feat-a".to_owned(),
                path: PathBuf::from("/work/repo.feat-a"),
                is_main: false,
            })
        );
    }

    #[test]
    fn parse_reads_a_create_row() {
        let line = Row::Create {
            name: "fix-parser".to_owned(),
        }
        .line("");

        let row = parse_row(&line);

        assert_eq!(
            row,
            Some(Row::Create {
                name: "fix-parser".to_owned()
            })
        );
    }

    #[test]
    fn shell_quote_escapes_a_single_quote() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
