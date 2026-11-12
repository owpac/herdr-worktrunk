//! The boxes of the popups of the plugin: a message with an OK button,
//! and a question with a Cancel button and an action button.
//!
//! A box uses the whole popup: a title, the message, and the buttons in
//! the middle. The colors come from the herdr theme (see theme.rs).

use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

use crate::theme;

const DEFAULT_ROWS: usize = 9;
const DEFAULT_COLUMNS: usize = 64;
/// The space on each side of the message.
const SIDE_MARGIN: usize = 2;
/// The space between 2 buttons.
const BUTTON_GAP: &str = "   ";

const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// The text on a colored background: the background of the panels of
/// herdr, else black.
fn on_color() -> theme::Color {
    match theme::palette().panel_bg {
        theme::Color::Reset => theme::Color::Black,
        color => color,
    }
}

/// The selected button: the dark text on the accent.
fn selected_style() -> String {
    let palette = theme::palette();
    format!(
        "{BOLD}{}{}",
        on_color().foreground(),
        palette.accent.background()
    )
}

/// The selected button that deletes or changes work: the dark text on red.
fn danger_selected_style() -> String {
    let palette = theme::palette();
    format!(
        "{BOLD}{}{}",
        on_color().foreground(),
        palette.red.background()
    )
}

/// A button that deletes or changes work, not selected.
fn danger_style() -> String {
    theme::palette().red.foreground()
}

/// The title of a box.
fn title_style() -> String {
    format!("{BOLD}{}", theme::palette().accent.foreground())
}

/// The hint under the buttons.
fn hint_style() -> String {
    theme::palette().overlay0.foreground()
}

/// A button of a box.
#[derive(Clone, Copy)]
struct Button<'label> {
    label: &'label str,
    /// A button that deletes or changes work: red.
    is_danger: bool,
}

/// Run `stty` on the terminal of the popup, and return its output.
fn stty(arguments: &[&str]) -> Option<String> {
    let output = Command::new("stty")
        .args(arguments)
        .stdin(Stdio::inherit())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The size of the terminal: rows, then columns.
pub(crate) fn terminal_size() -> (usize, usize) {
    let size = stty(&["size"]).unwrap_or_default();
    let mut numbers = size.split_whitespace().map(str::parse::<usize>);
    match (numbers.next(), numbers.next()) {
        (Some(Ok(rows)), Some(Ok(columns))) if rows > 0 && columns > 0 => (rows, columns),
        _ => (DEFAULT_ROWS, DEFAULT_COLUMNS),
    }
}

/// Cut `text` into lines of `width` characters or less, at the spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        // A word longer than the width (a path) breaks into pieces: the
        // popup cuts a line that is too long.
        let words = paragraph.split_whitespace().flat_map(|word| {
            let characters: Vec<char> = word.chars().collect();
            characters
                .chunks(width.max(1))
                .map(|piece| piece.iter().collect::<String>())
                .collect::<Vec<_>>()
        });
        for word in words {
            let word = word.as_str();
            let needed = line.chars().count() + word.chars().count() + 1;
            if !line.is_empty() && needed > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

/// Put `text` in the middle of a line of `width` characters.
fn centered(text: &str, visible_length: usize, width: usize) -> String {
    let padding = width.saturating_sub(visible_length) / 2;
    format!("{}{text}", " ".repeat(padding))
}

/// The terminal in raw mode: a key arrives without Enter, and without
/// echo. The terminal comes back to its mode when the value is dropped.
struct RawMode {
    saved: Option<String>,
}

impl RawMode {
    fn start() -> Self {
        let saved = stty(&["-g"]);
        let _ = stty(&["raw", "-echo"]);
        RawMode { saved }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            let _ = stty(&[saved.as_str()]);
        }
        let mut stdout = io::stdout();
        // The cursor is only for the user: a failed write has no effect.
        let _ = stdout.write_all(SHOW_CURSOR.as_bytes());
        let _ = stdout.flush();
    }
}

/// A key of the user, in raw mode.
#[derive(Debug, PartialEq, Eq)]
enum Key {
    Enter,
    Escape,
    Left,
    Right,
    Tab,
    Character(char),
    Other,
}

/// Read one key. A terminal sends an arrow key as 3 bytes in one read.
fn read_key() -> Key {
    let mut bytes = [0_u8; 8];
    let count = io::stdin().read(&mut bytes).unwrap_or(0);
    key_of(&bytes[..count])
}

fn key_of(bytes: &[u8]) -> Key {
    match bytes {
        // The end of the input: the same as Esc.
        [] | [0x1b] => Key::Escape,
        [b'\r'] | [b'\n'] => Key::Enter,
        [b'\t'] => Key::Tab,
        [0x1b, b'[', b'D'] | [0x1b, b'O', b'D'] => Key::Left,
        [0x1b, b'[', b'C'] | [0x1b, b'O', b'C'] => Key::Right,
        [byte] if byte.is_ascii_graphic() => Key::Character(char::from(*byte)),
        _ => Key::Other,
    }
}

/// The line of the buttons, and its visible length. The selected button
/// is in reverse video (red for a danger button). The other buttons are
/// in brackets (red text for a danger button).
fn button_line(buttons: &[Button], selected: usize) -> (String, usize) {
    let mut line = String::new();
    let mut length = 0;
    for (index, button) in buttons.iter().enumerate() {
        if index > 0 {
            line.push_str(BUTTON_GAP);
            length += BUTTON_GAP.len();
        }
        let text = format!("[ {} ]", button.label);
        length += text.chars().count();
        let style = match (index == selected, button.is_danger) {
            (true, true) => danger_selected_style(),
            (true, false) => selected_style(),
            (false, true) => danger_style(),
            (false, false) => String::new(),
        };
        line.push_str(&format!("{style}{text}{RESET}"));
    }
    (line, length)
}

/// The rows of a popup around its message: the title, the buttons, the
/// hint and 3 blank lines.
const FRAME_ROWS: usize = 6;

/// The width of the text of a message.
pub fn text_width() -> usize {
    let (_, columns) = terminal_size();
    columns.saturating_sub(2 * SIDE_MARGIN).max(1)
}

/// The lines of a message that the popup can show.
pub fn message_rows() -> usize {
    let (rows, _) = terminal_size();
    rows.saturating_sub(FRAME_ROWS).max(1)
}

/// Return true if the popup shows all the lines of `message`.
pub fn message_fits(message: &str) -> bool {
    wrap(message, text_width()).len() <= message_rows()
}

/// The lines that a popup of `rows` rows can show: the last one becomes
/// `…` if the message is longer. The title and the buttons stay visible.
fn fit_rows(mut lines: Vec<String>, rows: usize) -> Vec<String> {
    let available = rows.saturating_sub(FRAME_ROWS).max(1);
    if lines.len() > available {
        lines.truncate(available - 1);
        lines.push("…".to_owned());
    }
    lines
}

/// Draw a box: the title, the message, the buttons and a key hint.
fn draw(title: &str, message: &str, buttons: &[Button], selected: usize, hint: &str) {
    let (rows, columns) = terminal_size();
    let text_width = columns.saturating_sub(2 * SIDE_MARGIN).max(1);
    let message_lines = fit_rows(wrap(message, text_width), rows);
    // Title, blank line, message, blank line, buttons, blank line, hint.
    let content_rows = message_lines.len() + 6;
    let top_padding = rows.saturating_sub(content_rows) / 2;
    let (buttons_text, buttons_length) = button_line(buttons, selected);

    let mut screen = String::new();
    screen.push_str(CLEAR_SCREEN);
    screen.push_str(HIDE_CURSOR);
    screen.push_str(&"\r\n".repeat(top_padding));
    let title_text = format!("{}{title}{RESET}", title_style());
    screen.push_str(&centered(&title_text, title.chars().count(), columns));
    screen.push_str("\r\n\r\n");
    for line in &message_lines {
        screen.push_str(&centered(line, line.chars().count(), columns));
        screen.push_str("\r\n");
    }
    screen.push_str("\r\n");
    screen.push_str(&centered(&buttons_text, buttons_length, columns));
    screen.push_str("\r\n\r\n");
    let hint_text = format!("{}{hint}{RESET}", hint_style());
    screen.push_str(&centered(&hint_text, hint.chars().count(), columns));
    let mut stdout = io::stdout();
    // The box is only for the user: a failed write has no other effect.
    let _ = stdout.write_all(screen.as_bytes());
    let _ = stdout.flush();
}

fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Show `title` and `message` with an OK button, and wait for a key.
///
/// Outside a terminal, print the message on stderr.
pub fn show_message(title: &str, message: &str) {
    if !is_interactive() {
        eprintln!("{title}: {message}");
        return;
    }
    let _raw = RawMode::start();
    let ok = Button {
        label: "OK",
        is_danger: false,
    };
    draw(title, message, &[ok], 0, "any key closes");
    read_key();
}

/// Show `title` and `message` without a button, while a command works.
pub fn show_progress(title: &str, message: &str) {
    if is_interactive() {
        draw(title, message, &[], 0, "please wait");
    }
}

/// Print `message` under the output of a command, and wait for a key. The
/// output stays on the screen: the user can read it.
pub fn wait_after_output(message: &str) {
    if !is_interactive() {
        return;
    }
    print!(
        "\r\n{BOLD}{message}{RESET} {}· any key closes{RESET}",
        hint_style()
    );
    let _ = io::stdout().flush();
    let _raw = RawMode::start();
    read_key();
}

/// Ask a question with 2 buttons: Cancel (selected first) and `action`.
/// With `is_danger`, the action button is red.
///
/// ← → and Tab change the button, Enter presses it. `y` presses the
/// action button. `n`, `q` and Esc press Cancel. Outside a terminal, the
/// answer is no.
pub fn confirm(title: &str, message: &str, action: &str, is_danger: bool) -> bool {
    if !is_interactive() {
        return false;
    }
    let buttons = [
        Button {
            label: "Cancel",
            is_danger: false,
        },
        Button {
            label: action,
            is_danger,
        },
    ];
    let _raw = RawMode::start();
    let mut selected = 0;
    loop {
        let hint = "← → choose · Enter confirms · Esc cancels";
        draw(title, message, &buttons, selected, hint);
        match read_key() {
            Key::Enter => return selected == 1,
            Key::Escape | Key::Character('n' | 'N' | 'q') => return false,
            Key::Character('y' | 'Y') => return true,
            Key::Left | Key::Right | Key::Tab => selected = 1 - selected,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_cuts_the_lines_at_the_spaces() {
        let lines = wrap("This folder is not in a git repository.", 16);

        assert_eq!(lines, ["This folder is", "not in a git", "repository."]);
    }

    #[test]
    fn wrap_breaks_a_word_longer_than_the_width() {
        let lines = wrap("/Users/thomas/Workspace/a-very-long-folder", 10);

        assert_eq!(
            lines,
            ["/Users/tho", "mas/Worksp", "ace/a-very", "-long-fold", "er"]
        );
    }

    #[test]
    fn centered_puts_the_text_in_the_middle() {
        assert_eq!(centered("OK", 2, 10), "    OK");
    }

    #[test]
    fn key_of_reads_the_arrows_and_the_escape() {
        assert_eq!(key_of(&[0x1b, b'[', b'D']), Key::Left);
        assert_eq!(key_of(&[0x1b, b'[', b'C']), Key::Right);
        assert_eq!(key_of(&[0x1b]), Key::Escape);
        assert_eq!(key_of(b"\r"), Key::Enter);
        assert_eq!(key_of(b"y"), Key::Character('y'));
    }

    #[test]
    fn the_button_line_counts_only_the_visible_characters() {
        let buttons = [
            Button {
                label: "Cancel",
                is_danger: false,
            },
            Button {
                label: "Delete",
                is_danger: true,
            },
        ];

        let (line, length) = button_line(&buttons, 1);

        assert_eq!(
            length,
            "[ Cancel ]".len() + BUTTON_GAP.len() + "[ Delete ]".len()
        );
        assert!(line.contains(&format!("{}[ Delete ]", danger_selected_style())));
    }

    #[test]
    fn a_long_message_ends_with_an_ellipsis_and_keeps_the_frame() {
        let lines: Vec<String> = (1..=20).map(|line| line.to_string()).collect();

        let shown = fit_rows(lines.clone(), 12);

        assert_eq!(shown, ["1", "2", "3", "4", "5", "…"]);
        assert_eq!(fit_rows(lines[..3].to_vec(), 12), ["1", "2", "3"]);
    }
}
