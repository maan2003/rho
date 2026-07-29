//! Inert tree-sitter API for Zed's remote-buffer browser editor.
//!
//! Parsing never runs in the browser. The daemon supplies styled spans.

use std::{borrow::Cow, fmt};

#[derive(Clone, Debug, Default)]
pub struct Language;

#[derive(Clone, Debug, Default)]
pub struct Query {
    capture_names: Vec<&'static str>,
}

#[derive(Debug)]
pub struct QueryError;

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("tree-sitter is unavailable in the browser editor")
    }
}

impl std::error::Error for QueryError {}

#[derive(Clone, Debug)]
pub struct QueryProperty {
    pub key: Cow<'static, str>,
    pub value: Option<Box<str>>,
    pub capture_id: Option<usize>,
}

impl Query {
    pub fn new(_language: &Language, _source: &str) -> Result<Self, QueryError> {
        Ok(Self::default())
    }

    pub fn capture_names(&self) -> &[&str] {
        &self.capture_names
    }

    pub fn capture_index_for_name(&self, _name: &str) -> Option<u32> {
        None
    }

    pub fn pattern_count(&self) -> usize {
        0
    }

    pub fn property_settings(&self, _pattern_index: usize) -> &[QueryProperty] {
        &[]
    }
}
