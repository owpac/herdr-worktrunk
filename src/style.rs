//! The style of the popups picker and cleaner. It follows the design of
//! the plugins herdr-bar and herdr-newtab-plus: a prompt `❯`, a thin rule,
//! the selected row on a grey background after a `▌` in the accent color,
//! the secondary text in grey, and the hints of the keys at the bottom.
//!
//! The colors that carry a meaning are ANSI colors: they follow the theme
//! of the terminal. The greys are fixed 256 colors, as in the 2 plugins:
//! some themes give `bright_black` a color near the background.

use std::env;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::picker::shell_quote;
use crate::ui;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
/// The prompt, the pointer, the current tab of the preview, the scope.
pub const ACCENT: &str = "\x1b[38;5;12m";
/// The name of a repository.
pub const REPOSITORY: &str = "\x1b[38;5;14m";
/// A worktree without changes, and the row that creates a worktree.
pub const GREEN: &str = "\x1b[38;5;10m";
/// A worktree with changes that are not committed.
pub const YELLOW: &str = "\x1b[38;5;11m";
/// The secondary text: a commit subject, a folder, an age.
pub const MUTED: &str = "\x1b[38;5;249m";
/// The labels of the hints, and the tabs of the preview that are not
/// current.
pub const DIM: &str = "\x1b[38;5;246m";
/// The rules and the separators.
pub const RULE: &str = "\x1b[38;5;239m";
/// The background of the current tab of the preview.
pub const SELECTED_BACKGROUND: &str = "\x1b[48;5;237m";

/// The colors of fzf: the same values as the constants above. The header
/// shows a notice, in yellow. The selected row is not bold: fzf cannot
/// make only the branch bold, and the grey text is easier to read without
/// bold.
const FZF_COLORS: &str = "fg:-1,bg:-1,hl:12,fg+:-1:regular,bg+:237,hl+:12:bold,\
gutter:-1,pointer:12:bold,prompt:12:bold,query:-1:regular,info:246,spinner:12,\
separator:239,border:239,preview-border:239,ghost:246,header:11,\
footer:246,footer-border:239";

/// The width of the preview, in percent of the popup.
const PREVIEW_PERCENT: usize = 50;
/// The space on the left of the popup, as in the 2 plugins.
const LEFT_PADDING: usize = 1;
/// The columns of fzf before the text of a row: the pointer and the
/// marker.
const ROW_GUTTER: usize = 2;
/// The space between the left part and the right part of a row, and on
/// the right of a row.
const ROW_GAP: usize = 2;
/// The right part of a row goes away if the left part has less space.
const MIN_LEFT_WIDTH: usize = 16;
/// The separator between the parts of the right side of a row.
const SEPARATOR: &str = " · ";
/// The space between 2 hints of the footer.
const HINT_GAP: &str = "   ";

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// The fzf options of the 2 popups. `ghost` shows in the empty prompt,
/// `scope` shows on the right of the prompt, after the count, and
/// `hints` shows at the bottom.
pub fn fzf_arguments(ghost: &str, scope: &str, hints: &[(&str, &str)]) -> Vec<String> {
    let info = format!(
        "printf '%s  {BOLD}{ACCENT}%s{RESET}' \"$FZF_INFO\" {}",
        shell_quote(scope)
    );
    [
        "--ansi",
        "--layout",
        "reverse",
        "--no-sort",
        "--delimiter",
        "\t",
        "--with-nth",
        "4..",
        // sh starts faster than the shell of the user: fzf runs a command
        // at each key.
        "--with-shell",
        "sh -c",
        "--padding",
        "0,0,0,1",
        "--prompt",
        "❯ ",
        "--pointer",
        "▌",
        // fzf draws a ▌ in the gutter of each row: only the pointer shows
        // one in the 2 plugins.
        "--gutter",
        " ",
        "--highlight-line",
        "--ghost",
        ghost,
        "--info",
        "inline-right",
        "--info-command",
        &info,
        "--separator",
        "─",
        "--no-scrollbar",
        "--footer",
        &footer(hints),
        "--footer-border",
        "line",
        "--preview-window",
        &format!("right,{PREVIEW_PERCENT}%,border-left,noinfo"),
        "--color",
        FZF_COLORS,
    ]
    .iter()
    .map(|argument| (*argument).to_owned())
    .collect()
}

/// The hints of the keys: each key in grey, then its label in a darker
/// grey.
pub fn footer(hints: &[(&str, &str)]) -> String {
    hints
        .iter()
        .map(|(key, label)| format!("{MUTED}{key}{RESET}{DIM} {label}{RESET}"))
        .collect::<Vec<_>>()
        .join(HINT_GAP)
}

/// The variable that gives the columns of fzf to the command of its event
/// `start`: fzf gives it `FZF_COLUMNS=0`.
pub const COLUMNS_VARIABLE: &str = "TRUNK_COLUMNS";

/// The columns of fzf in the terminal of the popup: its width without the
/// padding. Call it before fzf starts.
pub fn terminal_columns() -> usize {
    ui::terminal_size().1.saturating_sub(LEFT_PADDING)
}

/// The columns of fzf in a command that fzf runs: `FZF_COLUMNS`, else
/// `TRUNK_COLUMNS`, else the width of the terminal.
pub fn fzf_columns() -> usize {
    let columns_of = |name: &str| {
        env::var(name)
            .ok()
            .and_then(|text| text.parse::<usize>().ok())
            .filter(|columns| *columns > 0)
    };
    columns_of("FZF_COLUMNS")
        .or_else(|| columns_of(COLUMNS_VARIABLE))
        .unwrap_or_else(terminal_columns)
}

/// The width of the text of a row, when fzf has `columns` columns: the
/// preview takes its part, then the pointer, then a gap on the right.
pub fn row_width(columns: usize) -> usize {
    let preview = columns * PREVIEW_PERCENT / 100;
    columns.saturating_sub(preview + ROW_GUTTER + ROW_GAP)
}

/// A part of a row: its color (or "") and its text.
pub struct Part {
    color: &'static str,
    text: String,
}

pub fn part(color: &'static str, text: impl Into<String>) -> Part {
    Part {
        color,
        text: text.into(),
    }
}

fn paint(part: &Part, text: &str) -> String {
    if part.color.is_empty() {
        text.to_owned()
    } else {
        format!("{}{text}{RESET}", part.color)
    }
}

/// Cut `parts` to `width` columns at most. The cut part ends with `…`.
/// Returns the text and its width.
fn cut(parts: &[Part], width: usize) -> (String, usize) {
    let mut text = String::new();
    let mut used = 0;
    for part in parts {
        let part_width = part.text.width();
        if used + part_width <= width {
            text.push_str(&paint(part, &part.text));
            used += part_width;
            continue;
        }
        // The last part that fits, cut, with … in the last column.
        let mut shown = String::new();
        for character in part.text.chars() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width + 1 > width {
                break;
            }
            shown.push(character);
            used += character_width;
        }
        if used < width {
            shown.push('…');
            used += 1;
        }
        text.push_str(&paint(part, &shown));
        break;
    }
    (text, used)
}

/// A row of a list in `width` columns: `left` at the start, `right` at
/// the end, with ` · ` between the parts of `right`. If the 2 sides do not
/// fit, the end of `left` is cut with `…`. In a small popup, `right` goes
/// away.
pub fn row(left: &[Part], right: &[Part], width: usize) -> String {
    let right: Vec<&Part> = right.iter().filter(|part| !part.text.is_empty()).collect();
    let right_width: usize = right.iter().map(|part| part.text.width()).sum::<usize>()
        + SEPARATOR.width() * right.len().saturating_sub(1);
    let has_right = !right.is_empty() && right_width + ROW_GAP + MIN_LEFT_WIDTH <= width;
    let left_space = if has_right {
        width - right_width - ROW_GAP
    } else {
        width
    };
    let (left_text, left_width) = cut(left, left_space);
    if !has_right {
        return left_text;
    }
    let separator = format!("{RULE}{SEPARATOR}{RESET}");
    let right_text = right
        .iter()
        .map(|part| paint(part, &part.text))
        .collect::<Vec<_>>()
        .join(&separator);
    let padding = width - left_width - right_width;
    format!("{left_text}{}{right_text}", " ".repeat(padding))
}

/// The tabs of the preview: the current tab in the accent color on a grey
/// background, the other tabs in grey.
pub fn tabs(names: &[&str], current: usize) -> String {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            if index == current {
                format!("{SELECTED_BACKGROUND}{BOLD}{ACCENT} {name} {RESET}")
            } else {
                format!("{DIM} {name} {RESET}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A horizontal rule of `width` columns.
pub fn rule(width: usize) -> String {
    format!("{RULE}{}{RESET}", "─".repeat(width))
}

/// The width of the preview: `FZF_PREVIEW_COLUMNS` in the preview
/// command of fzf.
pub fn preview_columns() -> usize {
    env::var("FZF_PREVIEW_COLUMNS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(40)
}

/// A short age: 5m, 3h, 12d.
pub fn age(seconds: u64) -> String {
    if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < 2 * DAY {
        format!("{}h", seconds / HOUR)
    } else {
        format!("{}d", seconds / DAY)
    }
}

/// Read a time of the form `2026-11-09T17:04:00Z` (or with an offset
/// `+01:00`), as seconds since 1970.
pub fn parse_time(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    if text.get(4..5) != Some("-") || text.get(10..11) != Some("T") {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Skip the fraction of a second, if any.
    let rest = text.get(19..)?;
    let zone =
        rest.trim_start_matches(|character: char| character == '.' || character.is_ascii_digit());
    let offset = match zone {
        "Z" | "" => 0,
        _ => {
            let sign = match zone.get(0..1)? {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(4..6)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()
}

/// The days from 1970-01-01 to a date of the Gregorian calendar (the
/// algorithm of Howard Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of `line` without the ANSI codes.
    fn visible(line: &str) -> String {
        let mut text = String::new();
        let mut in_code = false;
        for character in line.chars() {
            match (in_code, character) {
                (false, '\x1b') => in_code = true,
                (true, 'm') => in_code = false,
                (false, _) => text.push(character),
                _ => {}
            }
        }
        text
    }

    #[test]
    fn a_row_puts_the_right_side_at_the_end() {
        let line = row(
            &[part("", "feat-a"), part(MUTED, " — Add a parser")],
            &[part(MUTED, "repo.feat-a"), part(MUTED, "3h")],
            40,
        );

        let text = visible(&line);
        assert_eq!(text.width(), 40);
        assert!(text.starts_with("feat-a — Add a parser"));
        assert!(text.ends_with("repo.feat-a · 3h"));
    }

    #[test]
    fn a_long_left_side_ends_with_an_ellipsis() {
        let line = row(
            &[
                part("", "feat-a"),
                part(MUTED, " — a very long subject of a commit"),
            ],
            &[part(MUTED, "3h")],
            24,
        );

        let text = visible(&line);
        assert_eq!(text.width(), 24);
        assert!(text.starts_with("feat-a — a very lon…"), "{text}");
        assert!(text.ends_with("  3h"));
    }

    #[test]
    fn a_small_popup_drops_the_right_side() {
        let line = row(
            &[part("", "feat-a")],
            &[part(MUTED, "a-long-folder-name")],
            20,
        );

        assert_eq!(visible(&line), "feat-a");
    }

    #[test]
    fn the_width_counts_a_wide_character_twice() {
        let line = row(&[part("", "feat-a")], &[part(MUTED, "↑💬")], 30);

        assert_eq!(visible(&line).width(), 30);
    }

    #[test]
    fn the_row_width_leaves_the_preview_and_the_gutter() {
        assert_eq!(row_width(100), 46);
    }

    #[test]
    fn the_age_is_short() {
        assert_eq!(age(5 * MINUTE), "5m");
        assert_eq!(age(30 * HOUR), "30h");
        assert_eq!(age(12 * DAY + HOUR), "12d");
    }

    #[test]
    fn parse_time_reads_utc_and_an_offset() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_time("2026-11-09T17:04:00Z"), Some(1_794_243_840));
        assert_eq!(parse_time("2026-11-09T18:04:00+01:00"), Some(1_794_243_840));
        assert_eq!(parse_time("2026-11-09T17:04:00.250Z"), Some(1_794_243_840));
        assert_eq!(parse_time("not a time"), None);
    }

    #[test]
    fn the_footer_gives_each_key_and_its_label() {
        let text = visible(&footer(&[("⏎", "open"), ("esc", "close")]));

        assert_eq!(text, "⏎ open   esc close");
    }

    #[test]
    fn the_current_tab_has_the_accent_color() {
        let line = tabs(&["diff", "log"], 1);

        assert!(line.contains(&format!("{ACCENT} log ")));
        assert_eq!(visible(&line), " diff   log ");
    }
}
