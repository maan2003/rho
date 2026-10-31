//! The Python the GUI sends a workset, in one place so the user can change
//! it. The GUI runs no Python and never builds code from what it displays:
//! everything spliced in here is what the user typed or chose, quoted as a
//! Python string literal, or an id the directory gave.
//!
//! The names are the default policy's (`init.py`): `new_agent` creates an
//! agent in the workset it runs in and returns its id, and `rho.agent(id)`
//! acts on one.

use camino::Utf8Path;
use rho_agent_types::AgentId;

use crate::protocol::transcript::RuntimeKind;

/// What an agent runs on, as the policy's `runtime=` takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    pub kind: RuntimeKind,
    pub model: String,
    pub effort: String,
}

/// The models each runtime kind offers, and the efforts each takes, as
/// `_rho.pyi` lists them.
const NATIVE_MODELS: [&str; 3] = ["sol", "terra", "astra"];
const NATIVE_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const CLAUDE_MODELS: [&str; 3] = ["fable", "opus", "sonnet"];
const CLAUDE_EFFORTS: [&str; 3] = ["medium", "high", "xhigh"];

/// What the model picker cycles through, first the default. Any other
/// pair the policy takes can be typed.
pub const RUNTIMES: [&str; 6] = [
    "sol-medium",
    "sol-high",
    "terra-medium",
    "astra-high",
    "opus-high",
    "fable-high",
];

pub const DEFAULT_RUNTIME: &str = RUNTIMES[0];

impl Runtime {
    /// Reads `<model>-<effort>`; empty is the default.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        let text = if text.is_empty() {
            DEFAULT_RUNTIME.to_owned()
        } else {
            text
        };
        let refusal = || {
            format!(
                "unknown model `{text}`; write <model>-<effort>, like {}",
                RUNTIMES.join(", ")
            )
        };
        let (model, effort) = text.split_once('-').ok_or_else(refusal)?;
        let kind = if NATIVE_MODELS.contains(&model) && NATIVE_EFFORTS.contains(&effort) {
            RuntimeKind::Rho
        } else if CLAUDE_MODELS.contains(&model) && CLAUDE_EFFORTS.contains(&effort) {
            RuntimeKind::Claude
        } else {
            return Err(refusal());
        };
        Ok(Self {
            kind,
            model: model.to_owned(),
            effort: effort.to_owned(),
        })
    }

    /// How the picker and the field write it.
    pub fn label(&self) -> String {
        format!("{}-{}", self.model, self.effort)
    }

    /// The `runtime=` dict.
    fn python(&self) -> String {
        match self.kind {
            RuntimeKind::Rho => format!(
                "{{\"kind\": \"native\", \"model\": {}, \"effort\": {}, \"fast\": False}}",
                py_str(&self.model),
                py_str(&self.effort)
            ),
            RuntimeKind::Claude => format!(
                "{{\"kind\": \"claude\", \"model\": {}, \"effort\": {}}}",
                py_str(&self.model),
                py_str(&self.effort)
            ),
        }
    }
}

/// The picker's next choice after `current`.
pub fn cycle_runtime(current: &str) -> &'static str {
    let current = Runtime::parse(current).map(|runtime| runtime.label());
    let at = RUNTIMES
        .iter()
        .position(|runtime| Ok(*runtime) == current.as_deref());
    RUNTIMES[at.map_or(0, |at| (at + 1) % RUNTIMES.len())]
}

/// The first agent of a new isolated workset, on a clone of `repo` at
/// `rev` (empty: the remote's default branch). Prints its id.
pub fn new_agent_on(repo: &Utf8Path, rev: &str, runtime: &Runtime) -> String {
    format!(
        "print(new_agent(repo={}, rev={}, runtime={}))\n",
        py_str(repo.as_str()),
        py_str(rev),
        runtime.python()
    )
}

/// An agent working in `cwd` of the workset the code runs in: the first
/// of a host workset, or another beside an agent already there. Prints
/// its id.
pub fn new_agent_in(cwd: &Utf8Path, runtime: &Runtime) -> String {
    format!(
        "print(new_agent(cwd={}, runtime={}))\n",
        py_str(cwd.as_str()),
        runtime.python()
    )
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
    agent(id, &format!("set_runtime({})", runtime.python()))
}

/// Branches the agent's history before its `turns`-th last input from the
/// user.
pub fn rewind(id: AgentId, turns: u32) -> String {
    agent(id, &format!("rewind({turns})"))
}

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
        assert_eq!(
            Runtime::parse("").unwrap(),
            Runtime {
                kind: RuntimeKind::Rho,
                model: "sol".into(),
                effort: "medium".into()
            }
        );
        assert_eq!(
            Runtime::parse(" Opus-High ").unwrap().kind,
            RuntimeKind::Claude
        );
        assert!(Runtime::parse("opus-low").is_err(), "claude takes no low");
        assert!(Runtime::parse("med-eng").is_err());
        assert!(Runtime::parse("sol").is_err());
        assert_eq!(cycle_runtime("sol-medium"), "sol-high");
        assert_eq!(cycle_runtime(RUNTIMES[RUNTIMES.len() - 1]), RUNTIMES[0]);
        assert_eq!(
            cycle_runtime("terra-xhigh"),
            RUNTIMES[0],
            "off the list starts it"
        );
    }

    #[test]
    fn snippets_quote_what_they_splice() {
        let runtime = Runtime::parse("opus-high").unwrap();
        assert_eq!(
            new_agent_on("https://x/\"y\"".into(), "", &runtime),
            "print(new_agent(repo=\"https://x/\\\"y\\\"\", rev=\"\", \
             runtime={\"kind\": \"claude\", \"model\": \"opus\", \"effort\": \"high\"}))\n"
        );
        let id = AgentId::from_counter(3, &rho_agent_types::AgentIdDomain(1)).unwrap();
        assert_eq!(
            rewind(id, 2),
            format!("import rho\nrho.agent(\"{}\").rewind(2)\n", id.encoded())
        );
        assert!(
            set_runtime(id, &Runtime::parse("sol-low").unwrap())
                .contains("set_runtime({\"kind\": \"native\", \"model\": \"sol\", \"effort\": \"low\", \"fast\": False})")
        );
    }
}
