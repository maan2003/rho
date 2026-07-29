//! LSP data-model surface for the browser editor.
//!
//! The daemon owns language-server processes. Browser buffers retain protocol
//! value types for diagnostics, completions, and semantic data only.

pub use lsp_types::*;

use gpui::SharedString;
use serde::Serialize;
use std::{collections::HashMap, ffi::OsString, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LanguageServerId(pub usize);

impl LanguageServerId {
    pub fn from_proto(id: u64) -> Self {
        Self(id as usize)
    }
    pub fn to_proto(self) -> u64 {
        self.0 as u64
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LanguageServerName(pub SharedString);

impl std::fmt::Display for LanguageServerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for LanguageServerName {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

#[derive(Clone, Serialize)]
pub struct LanguageServerBinary {
    pub path: PathBuf,
    #[serde(skip)]
    pub arguments: Vec<OsString>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LanguageServerBinaryOptions {
    pub allow_path_lookup: bool,
    pub allow_binary_download: bool,
    pub pre_release: bool,
}

pub struct LanguageServer;

impl LanguageServer {
    pub fn full_capabilities() -> ClientCapabilities {
        ClientCapabilities::default()
    }
}
