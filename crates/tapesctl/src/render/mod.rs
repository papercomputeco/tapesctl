//! The human views: what a command prints when nobody asked for `--json`.
//!
//! One visual language for every command. Tables have no borders; columns are
//! aligned with two spaces and the header is dim caps, the way `gh` and
//! `docker ps` print. A record is a title, a dim subtitle, and aligned
//! label/value pairs. Colour carries meaning (a status word, an error) and
//! nothing else, and it is only applied when stdout is a terminal that wants
//! it. Piped output keeps the alignment, drops the colour, and writes `-` for
//! an absent value so `awk` still sees a field.
//!
//! Every view is a pure function of the document and a [`Theme`], so a
//! snapshot test can pin the layout at a fixed width with colour off. The
//! commands themselves only ever call `print!` on what comes back.

pub mod record;
pub mod style;
pub mod table;
pub mod text;

pub use record::Record;
pub use style::{Theme, Tone};
pub use table::{Align, Cell, Column, Table};
