//! A borderless table that fits the terminal.
//!
//! Columns declare a header, an alignment, a priority, and whether they may
//! flex. Layout is: measure every column at its natural width, and while the
//! total is wider than the theme allows, drop the lowest-priority droppable
//! column; then, if it still does not fit, shrink the flex column down to its
//! minimum. A row is never wrapped and never overflows the width by design.

use super::style::{Theme, Tone};
use super::text::{elide, width};

/// Which edge a column's text sits against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A column's shape.
#[derive(Debug, Clone)]
pub struct Column {
    /// Printed in dim caps on the first line.
    pub header: &'static str,
    pub align: Align,
    /// Lower is kept longer. Priority 0 columns are never dropped.
    pub priority: u8,
    /// Widest the column may grow, in cells, before its cells are elided.
    pub max: usize,
    /// The one column that gives up width before any column is dropped.
    /// Shrinks no further than `min`.
    pub flex: bool,
    /// Narrowest a flex column shrinks to.
    pub min: usize,
    /// Only shown when the terminal is at least this wide, whatever the
    /// priority says. For columns that are nice on a wide screen and noise on
    /// a narrow one.
    pub from_width: usize,
}

impl Column {
    /// A left-aligned column that is never dropped.
    #[must_use]
    pub const fn new(header: &'static str) -> Self {
        Self {
            header,
            align: Align::Left,
            priority: 0,
            max: 60,
            flex: false,
            min: 8,
            from_width: 0,
        }
    }

    #[must_use]
    pub const fn right(mut self) -> Self {
        self.align = Align::Right;
        self
    }

    #[must_use]
    pub const fn priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub const fn max(mut self, max: usize) -> Self {
        self.max = max;
        self
    }

    #[must_use]
    pub const fn flex(mut self, min: usize) -> Self {
        self.flex = true;
        self.min = min;
        self
    }

    #[must_use]
    pub const fn from_width(mut self, from_width: usize) -> Self {
        self.from_width = from_width;
        self
    }
}

/// One cell: text already sanitized, and the tone it should carry.
#[derive(Debug, Clone)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

impl Cell {
    #[must_use]
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }

    /// The cell for a value the server did not send.
    #[must_use]
    pub fn absent(theme: &Theme) -> Self {
        Self::new(theme.absent(), Tone::Muted)
    }

    /// A primary cell, or the absent glyph when `text` is empty.
    #[must_use]
    pub fn or_absent(text: String, tone: Tone, theme: &Theme) -> Self {
        if text.is_empty() {
            Self::absent(theme)
        } else {
            Self::new(text, tone)
        }
    }
}

/// The gap between columns, in cells.
const GAP: usize = 2;

/// A table ready to lay out.
#[derive(Debug, Default)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    #[must_use]
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// Add a row. A row shorter than the column list pads with absent cells;
    /// a longer one is truncated to the columns that exist.
    pub fn row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    /// Lay the table out for `theme` and render it, header first, every line
    /// newline-terminated.
    #[must_use]
    pub fn render(&self, theme: &Theme) -> String {
        let shown = self.choose_columns(theme);
        let widths = self.widths(&shown, theme);
        let mut out = String::new();

        let header: Vec<String> = shown
            .iter()
            .zip(&widths)
            .map(|(&i, &w)| {
                pad(
                    &self.columns[i].header.to_uppercase(),
                    w,
                    self.columns[i].align,
                )
            })
            .map(|h| theme.paint(Tone::Header, &h))
            .collect();
        out.push_str(header.join(&" ".repeat(GAP)).trim_end());
        out.push('\n');

        for row in &self.rows {
            let cells: Vec<String> = shown
                .iter()
                .zip(&widths)
                .map(|(&i, &w)| {
                    let cell = row.get(i);
                    let (text, tone) = match cell {
                        Some(c) => (elide(&c.text, w), c.tone),
                        None => (theme.absent().to_owned(), Tone::Muted),
                    };
                    theme.paint(tone, &pad(&text, w, self.columns[i].align))
                })
                .collect();
            out.push_str(cells.join(&" ".repeat(GAP)).trim_end());
            out.push('\n');
        }
        out
    }

    /// Indices of the columns that survive the width budget, in order.
    fn choose_columns(&self, theme: &Theme) -> Vec<usize> {
        let mut shown: Vec<usize> = (0..self.columns.len())
            .filter(|&i| theme.width >= self.columns[i].from_width)
            .collect();
        loop {
            let natural: usize = shown.iter().map(|&i| self.natural_width(i)).sum::<usize>()
                + GAP * shown.len().saturating_sub(1);
            // How much the flex column could give back before anything is
            // dropped.
            let slack: usize = shown
                .iter()
                .filter(|&&i| self.columns[i].flex)
                .map(|&i| self.natural_width(i).saturating_sub(self.columns[i].min))
                .sum();
            if natural.saturating_sub(slack) <= theme.width {
                return shown;
            }
            // Drop the lowest-priority droppable column; ties go to the
            // rightmost so the leading columns keep their place.
            let Some(victim) = shown
                .iter()
                .copied()
                .filter(|&i| self.columns[i].priority > 0)
                .max_by_key(|&i| (self.columns[i].priority, i))
            else {
                return shown;
            };
            shown.retain(|&i| i != victim);
        }
    }

    /// Final width of each shown column: natural, with the flex column
    /// shrunk to absorb any overflow.
    fn widths(&self, shown: &[usize], theme: &Theme) -> Vec<usize> {
        let mut widths: Vec<usize> = shown.iter().map(|&i| self.natural_width(i)).collect();
        let total: usize = widths.iter().sum::<usize>() + GAP * shown.len().saturating_sub(1);
        if total > theme.width {
            let over = total - theme.width;
            if let Some(pos) = shown.iter().position(|&i| self.columns[i].flex) {
                let min = self.columns[shown[pos]].min;
                // `min` decides when a column is dropped instead; once nothing is
                // left to drop, the flex column gives way down to a hard floor.
                widths[pos] = widths[pos].saturating_sub(over).max(min.min(8));
            }
        }
        widths
    }

    /// The width a column wants: its widest cell or header, capped at `max`.
    fn natural_width(&self, i: usize) -> usize {
        let column = &self.columns[i];
        let widest = self
            .rows
            .iter()
            .filter_map(|row| row.get(i))
            .map(|cell| width(&cell.text))
            .max()
            .unwrap_or(0)
            .max(width(column.header));
        widest.min(column.max)
    }
}

/// Pad `text` to `w` cells on the side its alignment leaves empty.
fn pad(text: &str, w: usize, align: Align) -> String {
    let fill = w.saturating_sub(width(text));
    match align {
        Align::Left => format!("{text}{}", " ".repeat(fill)),
        Align::Right => format!("{}{text}", " ".repeat(fill)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        let mut table = Table::new(vec![
            Column::new("title").flex(10).max(40),
            Column::new("status"),
            Column::new("turns").right().priority(2),
            Column::new("cost").right().priority(3),
            Column::new("id").priority(1),
        ]);
        table.row(vec![
            Cell::new("A fairly long session title here", Tone::Primary),
            Cell::new("completed", Tone::Good),
            Cell::new("19", Tone::Number),
            Cell::new("$41.62", Tone::Number),
            Cell::new("01a0d365-2f42-77a1-8473-bd2e295244a4", Tone::Secondary),
        ]);
        table.row(vec![
            Cell::new("Short", Tone::Primary),
            Cell::new("unknown", Tone::Muted),
            Cell::new("—", Tone::Muted),
            Cell::new("—", Tone::Muted),
            Cell::new("01a0d365-2895-77f7-9ac2-dad41f0a1577", Tone::Secondary),
        ]);
        table
    }

    #[test]
    fn a_wide_terminal_shows_every_column_aligned() {
        let rendered = sample().render(&Theme::plain(120));
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("TITLE"), "got: {rendered}");
        assert!(lines[0].contains("COST"), "got: {rendered}");
        // Right-aligned numbers share a right edge with their header.
        let end = lines[0].find("COST").unwrap() + 4;
        assert_eq!(&lines[1][end - 6..end], "$41.62");
        assert!(lines[0].trim_end().len() <= 120);
        assert!(!rendered.contains("│"));
    }

    #[test]
    fn a_narrow_terminal_drops_low_priority_columns_first() {
        let rendered = sample().render(&Theme::plain(70));
        let header = rendered.lines().next().unwrap();
        assert!(!header.contains("COST"), "got: {rendered}");
        assert!(header.contains("ID"), "got: {rendered}");
        assert!(header.contains("STATUS"), "got: {rendered}");
        for line in rendered.lines() {
            assert!(width(line) <= 70, "too wide: {line:?}");
        }
    }

    #[test]
    fn the_flex_column_shrinks_before_a_column_is_dropped() {
        // 32 + 2 + 9 + 2 + 5 + 2 + 6 + 2 + 36 = 96 at natural width; at 92
        // the title gives up 4 and nothing is dropped.
        let rendered = sample().render(&Theme::plain(92));
        let header = rendered.lines().next().unwrap();
        assert!(header.contains("COST"), "got: {rendered}");
        assert!(rendered.contains("…"), "got: {rendered}");
        for line in rendered.lines() {
            assert!(width(line) <= 92, "too wide: {line:?}");
        }
    }

    #[test]
    fn a_column_gated_on_width_stays_hidden_on_narrow_screens() {
        let mut table = Table::new(vec![
            Column::new("a"),
            Column::new("wide only").from_width(120),
        ]);
        table.row(vec![
            Cell::new("x", Tone::Primary),
            Cell::new("y", Tone::Primary),
        ]);
        assert!(!table.render(&Theme::plain(80)).contains("WIDE ONLY"));
        assert!(table.render(&Theme::plain(140)).contains("WIDE ONLY"));
    }

    #[test]
    fn a_short_row_pads_with_the_absent_glyph() {
        let mut table = Table::new(vec![Column::new("a"), Column::new("b")]);
        table.row(vec![Cell::new("x", Tone::Primary)]);
        assert!(table.render(&Theme::plain(80)).contains("x  —"));
        assert!(table.render(&Theme::piped()).contains("x  -"));
    }
}
