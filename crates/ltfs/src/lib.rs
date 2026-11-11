//! The Linear Tape File System (LTFS Format Specification 2.4) on LTO
//! tapes: [`label`]s, the [`index`] of folders, files and their extents,
//! and a mounted [`volume::Volume`] that reads files, appends files and
//! writes the index back. [`volume::format`] makes an empty volume.

pub mod index;
pub mod label;
pub mod names;
pub mod time;
pub mod volume;
pub mod xml;

pub use index::{Dir, Extent, File, Index, Location, Node, Times};
pub use label::Label;
pub use time::Time;
pub use volume::{format, probe, Error, Volume};

/// What this program writes as the creator of labels and indexes.
pub fn creator() -> String {
    format!("RustySalamander LTO Catalog {} - Windows", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests;
