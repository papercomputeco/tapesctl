//! A record view: one thing, in full.
//!
//! A title line, a dim subtitle, a blank line, then label/value pairs with
//! the labels padded to one width. Fields with nothing to say are left out
//! rather than shown as a dash; on a record, absence is silence. A trailing
//! "next" line names the command a reader would run from here.

use super::style::{Theme, Tone};
use super::text::{elide, sanitize, width};

/// One labelled value.
#[derive(Debug, Clone)]
struct Field {
    label: &'static str,
    value: String,
    tone: Tone,
}

/// A record ready to render.
#[derive(Debug, Default)]
pub struct Record {
    title: String,
    subtitle: Vec<String>,
    fields: Vec<Field>,
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

    /// Add a piece of the subtitle; pieces are joined with ` · `. Empty
    /// pieces are skipped.
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

    /// Add a field with a tone other than primary.
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

    /// Add a field only when there is a value.
    #[must_use]
    pub fn maybe(self, label: &'static str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.field(label, value),
            None => self,
        }
    }

    /// Name a command the reader could run next.
    #[must_use]
    pub fn next(mut self, command: impl Into<String>) -> Self {
        self.next.push(command.into());
        self
    }

    /// One field: the padded label, then the value. A multi-line value keeps
    /// its lines, each indented under the value column; a long single line is
    /// elided rather than wrapped, so the column stays readable.
    fn render_field(
        &self,
        field: &Field,
        label_width: usize,
        value_width: usize,
        theme: &Theme,
    ) -> String {
        let mut out = String::new();
        let label = format!("{:<label_width$}", field.label);
        out.push_str(&theme.paint(Tone::Secondary, &label));
        out.push_str("  ");
        for (i, line) in field.value.lines().enumerate() {
            if i > 0 {
                out.push('\n');
                out.push_str(&" ".repeat(label_width + 2));
            }
            out.push_str(&theme.paint(field.tone, &elide(&sanitize(line), value_width)));
        }
        out.push('\n');
        out
    }

    /// Render, every line newline-terminated.
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
            let label_width = self
                .fields
                .iter()
                .map(|f| width(f.label))
                .max()
                .unwrap_or(0);
            let value_width = theme.width.saturating_sub(label_width + 2).max(20);
            for field in &self.fields {
                out.push_str(&self.render_field(field, label_width, value_width, theme));
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
