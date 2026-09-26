//! Colour, width, and the terminal question.
//!
//! A [`Theme`] is decided once per command from stdout: is it a terminal, how
//! wide, and does the user want colour. Views take the theme and never look at
//! the environment themselves, which is what lets a test render at 80 columns
//! with colour off and compare bytes.

use std::io::IsTerminal;

use anstyle::{AnsiColor, Color, Style};

/// Width assumed when stdout is not a terminal, so piped output still has a
/// layout to align against rather than growing without bound.
pub const PIPED_WIDTH: usize = 100;

/// What a piece of text means, so the theme can decide how it looks.
///
/// Tones are semantic on purpose: a view says "this is a status word", never
/// "this is green", and the mapping lives in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The thing the row is about: a title, a value.
    Primary,
    /// Supporting text: an id, a path, a timestamp, a footer.
    Secondary,
    /// A column header.
    Header,
    /// A state that finished well: `completed`, `ok`.
    Good,
    /// A state still in motion: `live`, `running`.
    Active,
    /// A state that went wrong: `failed`, `error`.
    Bad,
    /// A state nobody has decided yet: `unknown`, `—`.
    Muted,
    /// A number with a unit: money, tokens.
    Number,
    /// The name of a command the user could run next.
    Command,
}

/// The per-invocation rendering decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Whether to emit ANSI styling.
    pub color: bool,
    /// Whether stdout is a terminal; decides absent-value glyphs and eliding.
    pub tty: bool,
    /// Columns available to a table.
    pub width: usize,
}

impl Theme {
    /// Decide from stdout and the environment.
    ///
    /// Colour is on only when stdout is a terminal, `NO_COLOR` is unset, and
    /// `TERM` is not `dumb`. Width comes from the terminal, or `COLUMNS`, or
    /// [`PIPED_WIDTH`].
    #[must_use]
    pub fn detect() -> Self {
        let tty = std::io::stdout().is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let dumb = std::env::var("TERM").is_ok_and(|term| term == "dumb");
        let width = terminal_size::terminal_size()
            .map(|(terminal_size::Width(w), _)| usize::from(w))
            .filter(|_| tty)
            .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
            .unwrap_or(PIPED_WIDTH);
        Self {
            color: tty && !no_color && !dumb,
            tty,
            width: width.max(40),
        }
    }

    /// A terminal of `width` columns with colour off: what tests render with.
    #[must_use]
    pub const fn plain(width: usize) -> Self {
        Self {
            color: false,
            tty: true,
            width,
        }
    }

    /// Not a terminal: no colour, `-` for absent values, [`PIPED_WIDTH`].
    #[must_use]
    pub const fn piped() -> Self {
        Self {
            color: false,
            tty: false,
            width: PIPED_WIDTH,
        }
    }

    /// The glyph for a value the server did not send.
    #[must_use]
    pub const fn absent(&self) -> &'static str {
        if self.tty { "—" } else { "-" }
    }

    /// Wrap `text` in the escapes for `tone`, or return it untouched when
    /// colour is off.
    #[must_use]
    pub fn paint(&self, tone: Tone, text: &str) -> String {
        if !self.color || text.is_empty() {
            return text.to_owned();
        }
        let style = style_for(tone);
        format!("{}{text}{}", style.render(), style.render_reset())
    }
}

/// The one place a tone becomes a colour.
fn style_for(tone: Tone) -> Style {
    let dim = Style::new().dimmed();
    match tone {
        Tone::Primary | Tone::Number => Style::new(),
        Tone::Secondary | Tone::Muted => dim,
        Tone::Header => dim,
        Tone::Good => Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))),
        Tone::Active => Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan))),
        Tone::Bad => Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
        Tone::Command => Style::new().bold(),
    }
}

/// The tone a status word carries, by what the word says.
///
/// The server's vocabulary is small and stable (`completed`, `ok`, `live`,
/// `running`, `failed`, `error`, `unknown`); anything else reads as primary so
/// a new word is visible rather than hidden.
#[must_use]
pub fn status_tone(status: &str) -> Tone {
    match status {
        "completed" | "ok" | "success" | "ended" => Tone::Good,
        "live" | "running" | "active" | "in_progress" => Tone::Active,
        "failed" | "error" | "errored" | "cancelled" | "canceled" => Tone::Bad,
        "unknown" | "" | "—" | "-" => Tone::Muted,
        _ => Tone::Primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_theme_paints_nothing() {
        assert_eq!(Theme::plain(80).paint(Tone::Good, "ok"), "ok");
    }

    #[test]
    fn a_coloured_theme_wraps_and_resets() {
        let theme = Theme {
            color: true,
            tty: true,
            width: 80,
        };
        let painted = theme.paint(Tone::Good, "ok");
        assert!(painted.starts_with("\x1b["), "got: {painted:?}");
        assert!(painted.ends_with("\x1b[0m"), "got: {painted:?}");
        assert!(painted.contains("ok"));
    }

    #[test]
    fn absent_glyph_follows_the_terminal() {
        assert_eq!(Theme::plain(80).absent(), "—");
        assert_eq!(Theme::piped().absent(), "-");
    }

    #[test]
    fn status_words_map_to_tones() {
        assert_eq!(status_tone("completed"), Tone::Good);
        assert_eq!(status_tone("live"), Tone::Active);
        assert_eq!(status_tone("failed"), Tone::Bad);
        assert_eq!(status_tone("unknown"), Tone::Muted);
        assert_eq!(status_tone("novel"), Tone::Primary);
    }
}
