//! The Python the GUI sends a workset, in one place so the user can change
//! it. The GUI runs no Python and never builds code from what it displays:
//! everything spliced in here is what the user typed or chose, quoted as a
//! Python string literal, or an id the directory gave.
//!
//! The names are the default policy's (`init.py`): `new_agent` creates an
//! agent in the workset it runs in and returns its id, and `rho.agent(id)`
//! acts on one. A draft may name another function in its place, which
//! takes the same arguments.

use camino::Utf8Path;
use rho_agent_types::{AgentId, MessageId, RuntimeKind};
pub use rho_agent_types::{DEFAULT_RUNTIME, RUNTIMES, RuntimeLabel as Runtime};

/// The policy function a new agent's code calls when the draft names none.
pub const DEFAULT_FUNCTION: &str = "new_agent";

/// A function of the workset namespace that makes an agent, such as
/// `new_agent` or one that init.py defines: a Python name, dotted or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function(String);

impl Function {
    /// Reads a dotted Python name; empty is the default.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self(DEFAULT_FUNCTION.to_owned()));
        }
        let identifier = |part: &str| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
        };
        if text.split('.').all(identifier) {
            Ok(Self(text.to_owned()))
        } else {
            Err(format!(
                "`{text}` is not a Python function name, such as `{DEFAULT_FUNCTION}`"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Function {
    fn default() -> Self {
        Self(DEFAULT_FUNCTION.to_owned())
    }
}

/// Existing agent start fields use the default when no answer was written.
pub fn parse_runtime(text: &str) -> Result<Runtime, String> {
    if text.trim().is_empty() {
        Ok(Runtime::default())
    } else {
        Runtime::parse(text)
    }
}

/// The policy's `runtime=` dict.
fn python_runtime(runtime: &Runtime) -> String {
    match runtime.kind() {
        RuntimeKind::Rho => format!(
            "{{\"kind\": \"native\", \"model\": {}, \"effort\": {}, \"fast\": False}}",
            py_str(runtime.model()),
            py_str(runtime.effort())
        ),
        RuntimeKind::Claude => format!(
            "{{\"kind\": \"claude\", \"model\": {}, \"effort\": {}}}",
            py_str(runtime.model()),
            py_str(runtime.effort())
        ),
    }
}

/// The picker's next choice after `current`.
pub fn cycle_runtime(current: &str) -> &'static str {
    let current = parse_runtime(current).map(|runtime| runtime.label());
    let at = RUNTIMES
        .iter()
        .position(|runtime| Ok(*runtime) == current.as_deref());
    RUNTIMES[at.map_or(0, |at| (at + 1) % RUNTIMES.len())]
}

/// The first agent of a new isolated workset, on a clone of `repo` at
/// `rev` (empty: the remote's default branch).
pub fn new_agent_on(
    function: &Function,
    repo: &Utf8Path,
    rev: &str,
    runtime: &Runtime,
    origin: Option<MessageId>,
) -> String {
    format!(
        "{}(repo={}, rev={}, runtime={}{})\n",
        function.as_str(),
        py_str(repo.as_str()),
        py_str(rev),
        python_runtime(runtime),
        origin_arg(origin)
    )
}

/// An agent working in `cwd` of the workset the code runs in: the first
/// of a host workset, or another beside an agent already there.
pub fn new_agent_in(
    function: &Function,
    cwd: &Utf8Path,
    runtime: &Runtime,
    origin: Option<MessageId>,
) -> String {
    format!(
        "{}(cwd={}, runtime={}{})\n",
        function.as_str(),
        py_str(cwd.as_str()),
        python_runtime(runtime),
        origin_arg(origin)
    )
}

/// `origin=`: the chat message an agent starts from. Policy keeps it as
/// the agent's `origin` prop, joins its thread, and hands it the message.
fn origin_arg(origin: Option<MessageId>) -> String {
    origin.map_or_else(String::new, |message| {
        format!(", origin={}", py_str(&format!("chat:{}", message.0.0)))
    })
}

fn agent(agent: AgentId, call: &str) -> String {
    format!(
        "import rho\nrho.agent({}).{call}\n",
        py_str(&agent.encoded())
    )
}

/// Compacts the agent's context at its next request.
pub fn compact(id: AgentId) -> String {
    agent(id, "compact()")
}

/// Moves the agent to another runtime, keeping its props.
pub fn set_runtime(id: AgentId, runtime: &Runtime) -> String {
    agent(id, &format!("set_runtime({})", python_runtime(runtime)))
}

/// Branches the agent's history before its `turns`-th last input from the
/// user.
/// Gives a native agent a fresh prompt cache key.
pub fn new_cache_key(id: AgentId) -> String {
    agent(id, "new_cache_key()")
}

/// `text` as a Python string literal that evaluates to exactly `text`:
/// quotes and backslashes escaped, every control character written as an
/// escape, everything else as it is (Python source is UTF-8).
pub fn py_str(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for char in text.chars() {
        match char {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            char if char.is_control() => {
                literal.push_str(&format!("\\U{:08x}", u32::from(char)));
            }
            char => literal.push(char),
        }
    }
    literal.push('"');
    literal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literals these must equal were read back by CPython's
    /// `ast.literal_eval` when they were written.
    #[test]
    fn a_string_literal_says_exactly_the_text() {
        assert_eq!(py_str(""), r#""""#);
        assert_eq!(
            py_str("she said \"\"\"hi\"\"\" \\ and left"),
            r#""she said \"\"\"hi\"\"\" \\ and left""#
        );
        assert_eq!(py_str("a\nb\r\tc"), r#""a\nb\r\tc""#);
        assert_eq!(
            py_str("nul\0 bell\x07 del\x7f"),
            r#""nul\U00000000 bell\U00000007 del\U0000007f""#
        );
        assert_eq!(py_str("next line\u{85}"), r#""next line\U00000085""#);
        assert_eq!(py_str("é 🦀 '"), "\"é 🦀 '\"");
        assert_eq!(py_str("\\N{BULLET} {x}"), r#""\\N{BULLET} {x}""#);
    }

    #[test]
    fn runtimes_parse_and_cycle() {
        assert_eq!(parse_runtime("").unwrap().as_str(), "opus-high");
        assert_eq!(
            Runtime::parse(" Opus-High ").unwrap().kind(),
            RuntimeKind::Claude
        );
        assert_eq!(
            Runtime::parse("opus-low").unwrap().kind(),
            RuntimeKind::Claude
        );
        assert_eq!(Runtime::parse("fable-max").unwrap().effort(), "max");
        assert!(Runtime::parse("sol-max").is_err(), "max is Claude-only");
        let haiku = Runtime::parse("haiku-medium").unwrap();
        assert_eq!((haiku.model(), haiku.effort()), ("haiku", "medium"));
        assert!(Runtime::parse("med-eng").is_err());
        assert!(Runtime::parse("sol").is_err());
        assert_eq!(cycle_runtime("sol-medium"), "sol-high");
        assert_eq!(cycle_runtime("astra-high"), "hydrogen-high");
        assert_eq!(cycle_runtime("hydrogen-high"), "neon-high");
        for effort in ["low", "medium", "high", "xhigh"] {
            let runtime = Runtime::parse(&format!("neon-{effort}")).unwrap();
            assert_eq!(runtime.kind(), RuntimeKind::Rho);
            assert_eq!((runtime.model(), runtime.effort()), ("neon", effort));
        }
        assert!(Runtime::parse("neon-max").is_err());
        assert_eq!(DEFAULT_RUNTIME, "opus-high");
        assert_eq!(parse_runtime("").unwrap().label(), "opus-high");
        assert_eq!(
            Runtime::parse("hydrogen-high").unwrap().kind(),
            RuntimeKind::Rho
        );
        assert_eq!(cycle_runtime(RUNTIMES[RUNTIMES.len() - 1]), RUNTIMES[0]);
        assert_eq!(
            cycle_runtime("terra-xhigh"),
            RUNTIMES[0],
            "off the list starts it"
        );
    }

    /// A function is a dotted Python name and nothing else, so the draft
    /// cannot splice other code into the call.
    #[test]
    fn a_function_is_a_python_name() {
        assert_eq!(Function::parse(" ").unwrap().as_str(), "new_agent");
        assert_eq!(
            Function::parse(" acme.roboman ").unwrap().as_str(),
            "acme.roboman"
        );
        assert_eq!(Function::parse("_start2").unwrap().as_str(), "_start2");
        for bad in ["2start", "a..b", "a.", "f()", "a b", "x;import os", "é"] {
            assert!(Function::parse(bad).is_err(), "{bad}");
        }
        let runtime = Runtime::parse("opus-high").unwrap();
        let roboman = Function::parse("roboman").unwrap();
        assert!(new_agent_in(&roboman, "/src".into(), &runtime, None).starts_with("roboman(cwd="));
    }

    #[test]
    fn snippets_quote_what_they_splice() {
        let runtime = Runtime::parse("opus-high").unwrap();
        assert_eq!(
            new_agent_on(
                &Function::default(),
                "https://x/\"y\"".into(),
                "",
                &runtime,
                None
            ),
            "new_agent(repo=\"https://x/\\\"y\\\"\", rev=\"\", \
             runtime={\"kind\": \"claude\", \"model\": \"opus\", \"effort\": \"high\"})\n"
        );
        let id = AgentId::from_counter(3, &rho_agent_types::AgentIdDomain(1)).unwrap();
        assert!(
            set_runtime(id, &Runtime::parse("sol-low").unwrap())
                .contains("set_runtime({\"kind\": \"native\", \"model\": \"sol\", \"effort\": \"low\", \"fast\": False})")
        );
    }
}
