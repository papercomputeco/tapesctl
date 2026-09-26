//! A record view: title, subtitle, aligned label/value pairs, and "next" hints.
//!
//! Empty fields are omitted rather than shown as a dash.

use super::style::{Theme, Tone};
use super::text::{elide, sanitize, width};

#[derive(Debug, Clone)]
struct Field {
    label: &'static str,
    value: String,
    tone: Tone,
}

#[derive(Debug, Clone, Copy)]
struct Columns {
    label: usize,
    value: usize,
}

impl Field {
    /// Multi-line values indent under the value column; long lines are elided,
    /// not wrapped.
    fn render(&self, columns: &Columns, theme: &Theme) -> String {
        let mut out = String::new();
        out.push_str(&theme.paint(
            Tone::Secondary,
            &format!("{:<width$}", self.label, width = columns.label),
        ));
        out.push_str("  ");
        for (i, line) in self.value.lines().enumerate() {
            if i > 0 {
                out.push('\n');
                out.push_str(&" ".repeat(columns.label + 2));
            }
            out.push_str(&theme.paint(self.tone, &elide(&sanitize(line), columns.value)));
        }
        out.push('\n');
        out
    }
}

#[derive(Debug, Default)]
pub struct Record {
    title: String,
    subtitle: Vec<String>,
    fields: Vec<Field>,
    blocks: Vec<(&'static str, String)>,
    next: Vec<String>,
}

impl Record {
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: sanitize(&title.into()),
            ..Self::default()
        }
    }

    /// Add a subtitle piece, joined with ` · `. Empty pieces are skipped.
    #[must_use]
    pub fn subtitle(mut self, piece: impl Into<String>) -> Self {
        let piece = sanitize(&piece.into());
        if !piece.is_empty() {
            self.subtitle.push(piece);
        }
        self
    }

    /// Add a field. An empty value is skipped.
    #[must_use]
    pub fn field(self, label: &'static str, value: impl Into<String>) -> Self {
        self.field_toned(label, value, Tone::Primary)
    }

    #[must_use]
    pub fn field_toned(
        mut self,
        label: &'static str,
        value: impl Into<String>,
        tone: Tone,
    ) -> Self {
        let value = value.into();
        if !value.is_empty() {
            self.fields.push(Field { label, value, tone });
        }
        self
    }

    #[must_use]
    pub fn maybe(self, label: &'static str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.field(label, value),
            None => self,
        }
    }

    /// A document printed whole after the fields, never elided so it can be copied.
    #[must_use]
    pub fn block(mut self, label: &'static str, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.blocks.push((label, text));
        }
        self
    }

    /// Name a command the reader could run next.
    #[must_use]
    pub fn next(mut self, command: impl Into<String>) -> Self {
        self.next.push(command.into());
        self
    }

    fn columns(&self, theme: &Theme) -> Columns {
        let label = self
            .fields
            .iter()
            .map(|f| width(f.label))
            .max()
            .unwrap_or(0);
        Columns {
            label,
            value: theme.width.saturating_sub(label + 2).max(20),
        }
    }

    #[must_use]
    pub fn render(&self, theme: &Theme) -> String {
        let mut out = String::new();
        if !self.title.is_empty() {
            out.push_str(&theme.paint(Tone::Command, &elide(&self.title, theme.width)));
            out.push('\n');
        }
        if !self.subtitle.is_empty() {
            let line = self.subtitle.join(" · ");
            out.push_str(&theme.paint(Tone::Secondary, &elide(&line, theme.width)));
            out.push('\n');
        }
        if !self.fields.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            let columns = self.columns(theme);
            for field in &self.fields {
                out.push_str(&field.render(&columns, theme));
            }
        }
        for (label, text) in &self.blocks {
            out.push('\n');
            out.push_str(&theme.paint(Tone::Secondary, label));
            out.push('\n');
            for line in text.lines() {
                out.push_str(&sanitize(line));
                out.push('\n');
            }
        }
        if !self.next.is_empty() {
            out.push('\n');
            for command in &self.next {
                out.push_str(&theme.paint(Tone::Secondary, "next  "));
                out.push_str(&theme.paint(Tone::Command, command));
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_record_lays_out_title_subtitle_and_aligned_fields() {
        let rendered = Record::new("PLG drip email campaign grove")
            .subtitle("01a0d365-2f42")
            .subtitle("")
            .subtitle("claude 2.1.280")
            .field("Status", "completed")
            .field("Turns", "19")
            .field("Model", "")
            .maybe("Cost", Some("$41.62".to_owned()))
            .maybe("Cwd", None)
            .next("tapesctl traces list 01a0d365-2f42")
            .render(&Theme::plain(80));
        assert_eq!(
            rendered,
            "PLG drip email campaign grove\n\
             01a0d365-2f42 · claude 2.1.280\n\
             \n\
             Status  completed\n\
             Turns   19\n\
             Cost    $41.62\n\
             \n\
             next  tapesctl traces list 01a0d365-2f42\n"
        );
    }

    #[test]
    fn a_multi_line_value_indents_under_the_value_column() {
        let rendered = Record::new("t")
            .field("Prompt", "first line\nsecond line")
            .render(&Theme::plain(80));
        assert!(
            rendered.contains("Prompt  first line\n        second line\n"),
            "got: {rendered}"
        );
    }
}
