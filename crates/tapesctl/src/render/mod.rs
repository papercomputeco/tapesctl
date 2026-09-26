//! Human views: what a command prints without `--json`.
//!
//! Colour is only applied on a terminal that wants it; piped output keeps the
//! alignment and writes `-` for an absent value so `awk` still sees a field.
//! Views are pure functions of the document and a [`Theme`] so tests can pin them.
pub mod record;
pub mod style;
pub mod table;
pub mod text;

pub use record::Record;
pub use style::{Theme, Tone};
pub use table::{Align, Cell, Column, Table};
