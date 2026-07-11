//! Code mode: the model orchestrates tools by writing JavaScript instead of
//! issuing individual tool calls.
//!
//! One [`CodeModeSession`] per agent owns a V8 isolate on a dedicated thread.
//! Each `exec` tool call evaluates raw JavaScript as a *cell* in V8 REPL mode
//! (notebook semantics: top-level bindings persist across cells). Nested
//! tools are exposed to scripts as async functions on the global `tools`
//! object and dispatched back through a [`ToolDispatcher`]. Long-running
//! cells yield after `yield_time_ms` and are resumed or terminated with the
//! `wait` tool.
//!
//! The model-facing tool surface (descriptions, pragma, yield/wait/terminate
//! semantics, status headers) matches OpenAI Codex code mode, except that
//! Codex's fresh-isolate + `store`/`load` state contract is replaced by the
//! persistent REPL scope.
//!
//! Scripts run with full host access through nested tools, in-process — the
//! same trust level as the shell tools; see SECURITY.md.

mod cell;
mod description;
mod runtime;
mod session;
mod truncate;

pub use description::{
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_YIELD_TIME_MS, EXEC_TOOL_NAME, NestedTool, WAIT_TOOL_NAME,
    exec_tool_spec, parse_exec_source, wait_tool_spec,
};
pub use session::{CodeModeSession, NestedToolOutput, ToolDispatcher, WaitArgs};
