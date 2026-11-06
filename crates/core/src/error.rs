//! What a failure means to the caller (#438): one classification that the CLI
//! exit code and the Python exception class are both read from.
//!
//! Every public error type in this crate implements [`Classify`].
//! [`kind_of`] finds the kind of any error that came out of this crate, so a
//! frontend holding a type-erased error (an `anyhow` chain, a `dyn Error`)
//! classifies it in one call instead of keeping its own list of downcasts.

use std::error::Error as StdError;

/// The class of a failure, from the caller's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// An option the caller must fix, refused before any work. The CLI exits
    /// 2, the code clap gives its own usage errors; Python raises
    /// `tylertoo.ConfigError`.
    Usage,
    /// The input data is the problem: a missing or mistyped column, an input
    /// the tiler cannot place on the map, a band archive that does not match
    /// its `--band` declaration. Python raises `ValueError`.
    Input,
    /// The machine lacks the memory this input needs. Python raises
    /// `MemoryError`.
    Resource,
    /// The run failed: I/O, a corrupt file, an internal error. Python raises
    /// `RuntimeError`.
    Runtime,
}

/// An error that knows its [`ErrorKind`].
pub trait Classify {
    /// What this failure means to the caller.
    fn kind(&self) -> ErrorKind;
}

/// The [`ErrorKind`] of `e` if it is one of this crate's error types, `None`
/// for any other error (an `io::Error`, a frontend's own message).
///
/// Only `e` itself is classified, not its `source()` chain: a caller walking
/// a chain (the CLI's `anyhow` errors) asks about each link.
pub fn kind_of(e: &(dyn StdError + 'static)) -> Option<ErrorKind> {
    macro_rules! first_kind {
        ($($ty:ty),+ $(,)?) => {
            $(if let Some(e) = e.downcast_ref::<$ty>() {
                return Some(e.kind());
            })+
        };
    }
    first_kind!(
        crate::Error,
        crate::archive_index::ArchiveError,
        crate::pmtiles_writer::WriteError,
        crate::pyramid::PyramidError,
        crate::overview::config::ConfigError,
        crate::overview::convert::ConvertError,
        crate::overview::export::ExportError,
        crate::overview::tiles::TilesError,
        crate::decode::DecodeError,
        crate::input::InputError,
    );
    None
}

/// The kind of a failure none of this crate's own variants classifies: an
/// input the read boundary refused as hostile (#789's page header) is the
/// input's problem, anything else a failed run.
pub(crate) fn fallback_kind(e: &(dyn StdError + 'static)) -> ErrorKind {
    match tylertoo_io::input_error(e) {
        Some(tylertoo_io::Error::HostilePageHeader) => ErrorKind::Input,
        _ => ErrorKind::Runtime,
    }
}
