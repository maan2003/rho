//! A Python notebook embedded in the host process, the only tool an agent
//! has. Cells, the commands they start and the host calls they make are all
//! sources: each reports on its own, under a session ID once it outlives the
//! report that first mentions it.

mod commands;
mod interpreter;
mod notebook;
mod runtime;
mod source;
#[cfg(test)]
mod tests;

pub use notebook::{CellHandle, Export, Notebook, Report, ToolCx, operation};
pub use source::{End, Kind, SessionId, SourceFacts, StreamProgress};

/// An image a report shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub media_type: String,
    pub data: Vec<u8>,
}
