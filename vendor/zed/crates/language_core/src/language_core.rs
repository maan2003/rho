// language_core: tree-sitter grammar infrastructure, LSP adapter traits,
// language configuration, and highlight mapping.

/// Identifies a running language server.
///
/// This identifier is part of the portable language data model. The native
/// `lsp` crate owns server processes, but buffers and rendered highlights also
/// carry server identities in browser builds.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct LanguageServerId(pub usize);

impl LanguageServerId {
    pub fn from_proto(id: u64) -> Self {
        Self(id as usize)
    }

    pub fn to_proto(self) -> u64 {
        self.0 as u64
    }
}

impl std::fmt::Display for LanguageServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub mod grammar;
pub mod highlight_map;
pub mod language_config;

pub use grammar::{
    BracketsConfig, BracketsPatternConfig, DebugVariablesConfig, DebuggerTextObject, Grammar,
    GrammarId, HighlightsConfig, IndentConfig, InjectionConfig, InjectionPatternConfig,
    NEXT_GRAMMAR_ID, OutlineConfig, OverrideConfig, OverrideEntry, RedactionConfig,
    RunnableCapture, RunnableConfig, TextObject, TextObjectConfig,
};
pub use highlight_map::{HighlightId, HighlightMap};
pub use language_config::{
    BlockCommentConfig, BracketPair, BracketPairConfig, BracketPairContent, DecreaseIndentConfig,
    JsxTagAutoCloseConfig, LanguageConfig, LanguageConfigOverride, LanguageMatcher,
    OrderedListConfig, Override, SoftWrap, TaskListConfig, WrapCharactersConfig, default_true,
    deserialize_regex, deserialize_regex_vec, regex_json_schema, regex_vec_json_schema,
    serialize_regex,
};

pub mod code_label;
pub mod language_name;
pub mod lsp_adapter;
pub mod manifest;
pub mod queries;
pub mod toolchain;

pub use code_label::{CodeLabel, CodeLabelBuilder, Symbol, SymbolKind};
pub use language_name::{LanguageId, LanguageName};
pub use lsp_adapter::{BinaryStatus, LanguageServerStatusUpdate, ServerHealth};
pub use manifest::ManifestName;
pub use queries::{LanguageQueries, QUERY_FILENAME_PREFIXES};
pub use toolchain::{Toolchain, ToolchainList, ToolchainMetadata, ToolchainScope};
