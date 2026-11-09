//! The worktree picker of the plugin: fzf over the worktrees and the
//! branches that `wt list` gives.
//!
//! Keys (the footer of the picker shows them):
//!
//! - `Enter` opens the selected worktree or branch. When the typed name
//!   matches no branch, the first row is
//!   `+ <name> — new worktree from the default branch`: `Enter` on this
//!   row creates the worktree.
//! - `⌫` asks to delete the selected worktree, when the filter is empty:
//!   the picker closes, the popup confirm asks the question, then the
//!   picker comes back. When the filter has text, `⌫` erases a character,
//!   as usual.
//! - `Tab` and `shift-Tab` change the tab of the preview: the diff since
//!   the branch left the default branch, the log, and the changes that
//!   are not committed. `shift-↑` and `shift-↓` scroll the preview.
//! - `Esc` cancels.
//!
//! fzf runs this binary for its events: `picker-rows` gives the rows for
//! the typed name, `picker-preview` gives the preview, `picker-tab`
//! changes the tab of the preview, and `picker-backspace` decides what
//! `⌫` does.
//!
//! The style comes from the module `style`: the design of herdr-bar and
//! herdr-newtab-plus.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::git;
use crate::herdr::Error;
use crate::stale;
use crate::style::{self, part, ACCENT, DIM, GREEN, MUTED, RED, RESET, RULE, YELLOW};

/// fzf exits with 130 when the user cancels.
pub(crate) const FZF_CANCEL_EXIT_CODE: i32 = 130;
/// fzf exits with 1 when no row matches.
pub(crate) const FZF_NO_MATCH_EXIT_CODE: i32 = 1;
const GHOST: &str = "worktree — type a name to filter, or a new name to create it";
// The footer has the width of the list: the arrows need no hint.
const HINTS: [(&str, &str); 4] = [
    ("⏎", "open or create"),
    ("⌫", "delete (empty filter)"),
    ("⇥", "preview"),
    ("esc", "close"),
];
/// The variable that gives the checkout of the popup to the rows command:
/// its row gets the label `current`.
const CURRENT_VARIABLE: &str = "TRUNK_CURRENT_CHECKOUT";
/// The first line that fzf prints when `⌫` asks for a deletion.
const DELETE_REQUEST: &str = "delete";
const PREVIEW_COMMIT_COUNT: &str = "20";
/// The tabs of the preview, in the order of `Tab`.
const PREVIEW_TABS: [&str; 3] = ["diff", "log", "changes"];

/// The choice of the user in the picker.
#[derive(Debug, PartialEq, Eq)]
pub enum Picked {
    /// Open this row (or create it).
    Open(Row),
    /// Delete the worktree of this row, after a question.
    Delete(Row),
}

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

    fn path(&self) -> String {
        match self {
            Row::Worktree { path, .. } => path.display().to_string(),
            _ => String::new(),
        }
    }

    /// The line of fzf: kind, branch, path and text, separated by tabs.
    /// The text fills `width` columns. On the left: the state, the branch
    /// and the subject of its last commit. On the right: the folder,
    /// `main` or `current`, the lines and the commits since the default
    /// branch, the marker of wt and the age of the last commit. A small
    /// popup drops the folder first.
    fn line(&self, details: &Details, now: u64, width: usize) -> String {
        let (state, mut right) = match self {
            Row::Create { .. } => (part(GREEN, "+ "), Vec::new()),
            Row::Worktree { path, is_main, .. } => {
                let state = if details.has_changes {
                    part(YELLOW, "● ")
                } else {
                    part(GREEN, "✓ ")
                };
                // The folder name is sufficient: the full path is long.
                let mut right = vec![part(MUTED, git::folder_name(path))];
                if *is_main {
                    right.push(part(ACCENT, "main"));
                } else if details.is_current {
                    right.push(part(ACCENT, "current"));
                }
                (state, right)
            }
            Row::Branch { .. } => (part(DIM, "○ "), vec![part(DIM, "no worktree")]),
        };
        let mut left = vec![state, part("", self.branch())];
        let subject = match self {
            Row::Create { .. } => "new worktree from the default branch",
            _ => details.subject.as_str(),
        };
        if !subject.is_empty() {
            left.push(part(RULE, " — "));
            left.push(part(MUTED, subject));
        }
        right.push(part("", details.lines_text()));
        right.push(part("", details.commits_text()));
        right.push(part("", details.marker.clone()));
        if let Some(time) = details.committed_at {
            right.push(part(MUTED, style::age(now.saturating_sub(time))));
        }
        let text = style::row(&left, &right, width);
        format!(
            "{}\t{}\t{}\t{text}",
            self.kind(),
            self.branch(),
            self.path()
        )
    }
}

/// What the picker shows about a worktree or a branch, from `wt list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Details {
    /// The subject of the last commit.
    subject: String,
    /// The commits ahead of the default branch.
    ahead: u64,
    /// The commits behind the default branch.
    behind: u64,
    /// The lines added since the default branch.
    added: u64,
    /// The lines deleted since the default branch.
    deleted: u64,
    /// The marker of wt, for example the state of an agent (`💬`).
    marker: String,
    /// The time of the last commit, in seconds since 1970.
    committed_at: Option<u64>,
    /// The worktree has changes that are not committed.
    has_changes: bool,
    /// The worktree of the folder of the popup.
    is_current: bool,
}

impl Details {
    /// The lines since the default branch, as wt shows them: `+804 -163`.
    fn lines_text(&self) -> String {
        let mut texts = Vec::new();
        if self.added > 0 {
            texts.push(format!("{GREEN}+{}{RESET}", self.added));
        }
        if self.deleted > 0 {
            texts.push(format!("{RED}-{}{RESET}", self.deleted));
        }
        texts.join(" ")
    }

    /// The commits since the default branch, as wt shows them: `↑1 ↓2`.
    fn commits_text(&self) -> String {
        let mut texts = Vec::new();
        if self.ahead > 0 {
            texts.push(format!("{GREEN}↑{}{RESET}", self.ahead));
        }
        if self.behind > 0 {
            texts.push(format!("{RED}↓{}{RESET}", self.behind));
        }
        texts.join(" ")
    }
}

/// A row of the cache: the row and its details, in 12 fields separated by
/// tabs. fzf does not read it: `print_rows` makes the lines of fzf with
/// the width of the popup.
fn record(row: &Row, details: &Details) -> String {
    let clean = |text: &str| text.replace(['\t', '\n'], " ");
    [
        row.kind().to_owned(),
        clean(row.branch()),
        row.path(),
        clean(&details.subject),
        details.ahead.to_string(),
        details.behind.to_string(),
        details.added.to_string(),
        details.deleted.to_string(),
        clean(&details.marker),
        details
            .committed_at
            .map(|time| time.to_string())
            .unwrap_or_default(),
        u8::from(details.has_changes).to_string(),
        u8::from(details.is_current).to_string(),
    ]
    .join("\t")
}

/// Read a row of the cache.
fn parse_record(line: &str) -> Option<(Row, Details)> {
    let row = parse_row(line)?;
    let fields: Vec<&str> = line.split('\t').collect();
    let field = |index: usize| fields.get(index).copied().unwrap_or_default();
    let number = |index: usize| field(index).parse().unwrap_or(0);
    let details = Details {
        subject: field(3).to_owned(),
        ahead: number(4),
        behind: number(5),
        added: number(6),
        deleted: number(7),
        marker: field(8).to_owned(),
        committed_at: field(9).parse().ok(),
        has_changes: field(10) == "1",
        is_current: field(11) == "1",
    };
    Some((row, details))
}

/// Read the rows of `wt list --format json --branches`, as rows of the
/// cache. `current` is the checkout of the popup: wt runs in the main
/// checkout, so its own `current` flag is always on the main checkout.
fn list_records(repository: &Path, current: Option<&Path>) -> Result<Vec<String>, Error> {
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
    let current = current.map(git::canonical);
    let items = json["items"].as_array().cloned().unwrap_or_default();
    let mut records = Vec::new();
    for item in items {
        let Some(branch) = item["branch"].as_str() else {
            continue;
        };
        let changes = &item["worktree"]["changes"];
        let has_changes = [
            "staged",
            "modified",
            "untracked",
            "renamed",
            "deleted",
            "conflicted",
        ]
        .iter()
        .any(|kind| changes[kind].as_bool().unwrap_or(false));
        let path = item["worktree"]["path"].as_str().map(PathBuf::from);
        let number = |value: &Value| value.as_u64().unwrap_or(0);
        let default_branch = &item["default_branch"];
        let details = Details {
            subject: item["head"]["subject"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            ahead: number(&default_branch["ahead"]),
            behind: number(&default_branch["behind"]),
            added: number(&default_branch["diff"]["added"]),
            deleted: number(&default_branch["diff"]["deleted"]),
            marker: item["marker"].as_str().unwrap_or_default().to_owned(),
            committed_at: item["head"]["committed_at"]
                .as_str()
                .and_then(style::parse_time),
            has_changes,
            is_current: match (&path, &current) {
                (Some(path), Some(current)) => git::canonical(path) == *current,
                _ => false,
            },
        };
        let row = match path {
            Some(path) => Row::Worktree {
                branch: branch.to_owned(),
                path,
                is_main: item["worktree"]["main"].as_bool().unwrap_or(false),
            },
            None => Row::Branch {
                branch: branch.to_owned(),
            },
        };
        records.push(record(&row, &details));
    }
    Ok(records)
}

/// The lines of fzf for the typed name, in `width` columns: the create
/// row first (if the name is new), then the rows of `wt list`.
pub fn rows_for_query(records: &[String], query: &str, width: usize, now: u64) -> Vec<String> {
    // A tab separates the fields of a row: a name cannot contain it.
    let name = query.trim().replace('\t', " ");
    let rows: Vec<(Row, Details)> = records
        .iter()
        .filter_map(|line| parse_record(line))
        .collect();
    let is_known_branch = rows.iter().any(|(row, _)| row.branch() == name);
    let mut lines = Vec::new();
    if !name.is_empty() && !is_known_branch {
        lines.push(Row::Create { name }.line(&Details::default(), now, width));
    }
    lines.extend(
        rows.iter()
            .map(|(row, details)| row.line(details, now, width)),
    );
    lines
}

/// Put `text` in single quotes for the shell that runs the fzf commands.
pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The cache of the rows of `wt list`, for the events of fzf.
fn cache_path() -> PathBuf {
    env::temp_dir().join(format!("herdr-worktrunk-picker-{}.tsv", std::process::id()))
}

fn write_cache(cache: &Path, repository: &Path) -> Result<Vec<String>, Error> {
    let current = env::var_os(CURRENT_VARIABLE).map(PathBuf::from);
    let records = list_records(repository, current.as_deref())?;
    fs::write(cache, records.join("\n"))
        .map_err(|error| Error::Command(format!("cannot write {}: {error}", cache.display())))?;
    Ok(records)
}

fn read_cache(cache: &Path) -> Vec<String> {
    fs::read_to_string(cache)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// fzf event `picker-rows`: print the rows for the typed name, with the
/// width of the popup (fzf runs it again when the popup changes size).
///
/// The first call reads `wt list` and writes the cache: fzf shows the
/// picker at once, with a spinner until the rows arrive. A key during the
/// load cancels this call, so each call loads the rows if the cache is
/// missing.
pub fn print_rows(repository: &Path, cache: &Path, query: &str) {
    if !cache.exists() {
        // Without rows, the picker still offers the create row.
        let _ = write_cache(cache, repository);
    }
    let width = style::row_width(style::fzf_columns());
    let mut stdout = io::stdout().lock();
    for row in rows_for_query(&read_cache(cache), query, width, stale::now()) {
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

/// The current tab of a preview with `count` tabs.
pub(crate) fn read_tab(cache: &Path, count: usize) -> usize {
    fs::read_to_string(tab_path(cache))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|tab| *tab < count)
        .unwrap_or(0)
}

/// fzf event `picker-tab`: go to the next (or the previous) tab of the
/// preview, and print the fzf action that shows it. The first line of the
/// preview shows the tabs.
pub fn change_tab(cache: &Path, is_next: bool, count: usize) {
    let count = count.max(1);
    let current = read_tab(cache, count);
    let tab = if is_next {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    };
    // Without the file, the preview stays on its first tab.
    let _ = fs::write(tab_path(cache), tab.to_string());
    println!("refresh-preview");
}

/// Run a command for the preview, and print its output and its errors.
fn print_command(command: &mut Command) {
    // GIT_OPTIONAL_LOCKS=0: git status and git diff do not write the index.
    // A write changes the last activity of the worktree, and can block an
    // agent that works in it.
    let output = command
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output();
    match output {
        Ok(output) => {
            print!("{}", String::from_utf8_lossy(&output.stdout));
            print!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Err(error) => println!("cannot run the preview: {error}"),
    }
}

/// fzf event `picker-preview`: print the tabs, a rule, then the current
/// tab of the preview.
pub fn print_preview(repository: &Path, cache: &Path, kind: &str, branch: &str, path: &str) {
    if kind == "create" {
        println!(
            "{MUTED}⏎ creates the worktree {branch} from the default branch.{}",
            style::RESET
        );
        return;
    }
    let tab = read_tab(cache, PREVIEW_TABS.len());
    println!("{}", style::tabs(&PREVIEW_TABS, tab));
    println!("{}", style::rule(style::preview_columns()));
    print_tab(repository, PREVIEW_TABS[tab], branch, path);
}

/// Print the content of the tab `name` of the preview: `diff`, `log` or
/// `changes`.
pub(crate) fn print_tab(repository: &Path, name: &str, branch: &str, path: &str) {
    match name {
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
        _ if path.is_empty() => println!(
            "{MUTED}{branch} has no worktree: no changes to show.{}",
            style::RESET
        ),
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

/// fzf event `picker-backspace`: print the fzf actions of `⌫`.
///
/// With text in the filter, `⌫` erases a character. With an empty filter,
/// on a worktree, it ends the picker with a deletion request. On another
/// row, the header tells why nothing happens, until the next key.
pub fn backspace_action(query: &str, kind: &str, branch: &str) {
    let actions = if !query.is_empty() {
        "backward-delete-char".to_owned()
    } else {
        match kind {
            "worktree" => format!("print({DELETE_REQUEST})+accept"),
            "main" => "change-header:The main checkout cannot be deleted.".to_owned(),
            _ => format!("change-header:{branch} has no worktree to delete."),
        }
    };
    println!("{actions}");
}

/// Show the picker for `repository`, and return the choice of the user.
///
/// Returns None if the user cancels.
pub fn pick(repository: &Path) -> Result<Option<Picked>, Error> {
    let main_checkout = git::checkout_of(repository)
        .map(|checkout| checkout.main_checkout())
        .unwrap_or_else(|| repository.to_path_buf());
    let cache = cache_path();
    let _ = fs::remove_file(&cache);
    let executable = env::current_exe()
        .map_err(|error| Error::Command(format!("cannot find the binary: {error}")))?;
    let binary = shell_quote(&executable.to_string_lossy());
    let repository_text = shell_quote(&main_checkout.to_string_lossy());
    let cache_text = shell_quote(&cache.to_string_lossy());
    let rows_command = format!("{binary} picker-rows {repository_text} {cache_text} {{q}}");
    let _ = fs::remove_file(tab_path(&cache));
    let fzf = Command::new("fzf")
        .env(
            style::COLUMNS_VARIABLE,
            style::terminal_columns().to_string(),
        )
        .env(CURRENT_VARIABLE, repository)
        .args(style::fzf_arguments(
            GHOST,
            &git::folder_name(&main_checkout),
            &HINTS,
        ))
        .arg("--preview")
        .arg(format!(
            "{binary} picker-preview {repository_text} {cache_text} {{1}} {{2}} {{3}}"
        ))
        .arg("--bind")
        .arg(format!("start:reload({rows_command})"))
        .arg("--bind")
        .arg(format!("change:reload({rows_command})+change-header:"))
        .arg("--bind")
        .arg(format!("resize:reload({rows_command})"))
        .arg("--bind")
        .arg(format!(
            "bspace:transform({binary} picker-backspace {{q}} {{1}} {{2}})"
        ))
        .arg("--bind")
        .arg(format!(
            "tab:transform({binary} picker-tab {cache_text} next 3)"
        ))
        .arg("--bind")
        .arg(format!(
            "btab:transform({binary} picker-tab {cache_text} previous 3)"
        ))
        // The event start loads the rows.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Command(format!("cannot run fzf: {error}")))?;
    let output = fzf
        .wait_with_output()
        .map_err(|error| Error::Command(format!("fzf failed: {error}")))?;
    let _ = fs::remove_file(&cache);
    let _ = fs::remove_file(tab_path(&cache));
    match output.status.code() {
        Some(0) => {}
        Some(FZF_CANCEL_EXIT_CODE) | Some(FZF_NO_MATCH_EXIT_CODE) => return Ok(None),
        _ => return Err(Error::Command(style::FZF_FAILED.to_owned())),
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    if first == DELETE_REQUEST {
        return Ok(lines.next().and_then(parse_row).map(Picked::Delete));
    }
    Ok(parse_row(first).map(Picked::Open))
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

    const WIDTH: usize = 60;
    const NOW: u64 = 1_000_000;

    fn worktree() -> (Row, Details) {
        (
            Row::Worktree {
                branch: "feat-a".to_owned(),
                path: PathBuf::from("/work/repo.feat-a"),
                is_main: false,
            },
            Details {
                subject: "Add a parser".to_owned(),
                ahead: 2,
                behind: 1,
                added: 30,
                deleted: 4,
                marker: "💬".to_owned(),
                committed_at: Some(NOW - 3 * 3600),
                has_changes: true,
                is_current: true,
            },
        )
    }

    fn worktree_record() -> String {
        let (row, details) = worktree();
        record(&row, &details)
    }

    #[test]
    fn a_new_name_gets_the_create_row_first() {
        let rows = rows_for_query(&[worktree_record()], "fix-parser", WIDTH, NOW);

        assert!(rows[0].starts_with("create\tfix-parser\t\t"));
        assert!(rows[1].starts_with("worktree\tfeat-a\t/work/repo.feat-a\t"));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_known_branch_gets_no_create_row() {
        let rows = rows_for_query(&[worktree_record()], "feat-a", WIDTH, NOW);

        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("worktree\tfeat-a\t"));
    }

    #[test]
    fn an_empty_query_gets_no_create_row() {
        let rows = rows_for_query(&[worktree_record()], "  ", WIDTH, NOW);

        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn a_tab_in_the_name_does_not_break_the_row() {
        let rows = rows_for_query(&[], "a\tb", WIDTH, NOW);

        assert!(rows[0].starts_with("create\ta b\t"));
    }

    #[test]
    fn a_record_keeps_the_row_and_its_details() {
        let (row, details) = worktree();

        assert_eq!(parse_record(&record(&row, &details)), Some((row, details)));
    }

    #[test]
    fn the_text_of_a_worktree_gives_its_state_subject_folder_and_age() {
        let (row, details) = worktree();

        let line = row.line(&details, NOW, 90);
        let text = line.split('\t').nth(3).unwrap();

        let text = style::visible(text);
        assert!(text.starts_with("● feat-a — Add a parser"), "{text}");
        assert!(
            text.ends_with("repo.feat-a · current · +30 -4 · ↑2 ↓1 · 💬 · 3h"),
            "{text}"
        );
    }

    #[test]
    fn parse_reads_a_worktree_row_and_its_path() {
        let (row, details) = worktree();
        let line = row.line(&details, NOW, WIDTH);

        assert_eq!(parse_row(&line), Some(row));
    }

    #[test]
    fn parse_reads_a_create_row() {
        let line = Row::Create {
            name: "fix-parser".to_owned(),
        }
        .line(&Details::default(), NOW, WIDTH);

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
