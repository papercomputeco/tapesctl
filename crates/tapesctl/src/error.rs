//! Error type for the `tapesctl` CLI.

use snafu::Snafu;

/// Convenience alias defaulting the error to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors surfaced by the CLI.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// A command is wired up but its implementation has not landed yet.
    #[snafu(display("{what} is not implemented yet"))]
    NotImplemented {
        /// The command that was invoked.
        what: &'static str,
    },
}
