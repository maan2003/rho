//! Generated Claude Code settings for agents that must run with Rho's tools
//! only.
//!
//! Claude's `permissions.deny` list removes the named tools from the model
//! entirely, even under `bypassPermissions`, but it is a blocklist: a
//! built-in tool it does not name stays available. [`BUILTIN_TOOLS`] is
//! therefore maintained by hand against the CLI version Rho runs, and a new
//! CLI tool shows up for the model until the list is extended.

use serde_json::{Value, json};

/// Every tool the Claude Code CLI provides on its own, plus `ToolSearch`
/// (denied as well as switched off by `ENABLE_TOOL_SEARCH=false`, so an MCP
/// tool is never deferred behind it) and `WaitForMcpServers`, which appears
/// once tool search is off.
pub const BUILTIN_TOOLS: &[&str] = &[
    "Agent",
    "Bash",
    "BashOutput",
    "CronCreate",
    "CronDelete",
    "CronList",
    "DesignSync",
    "Edit",
    "EnterWorktree",
    "ExitPlanMode",
    "ExitWorktree",
    "Glob",
    "Grep",
    "KillShell",
    "LS",
    "ListAgents",
    "Monitor",
    "MultiEdit",
    "NotebookEdit",
    "PushNotification",
    "Read",
    "RemoteTrigger",
    "ReportFindings",
    "ScheduleWakeup",
    "SendMessage",
    "Skill",
    "Task",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskUpdate",
    "TodoWrite",
    "ToolSearch",
    "WaitForMcpServers",
    "WebFetch",
    "WebSearch",
    "Workflow",
    "Write",
];

/// `base` with every built-in tool and every tool of the named MCP servers
/// added to `permissions.deny`, leaving the account's other settings alone.
/// A non-object base is replaced rather than merged.
pub fn deny_all_but_own_tools(base: &Value, deny_mcp_servers: &[&str]) -> Value {
    let mut settings = match base {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let permissions = settings.entry("permissions").or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }
    let deny = permissions
        .as_object_mut()
        .expect("permissions is an object")
        .entry("deny")
        .or_insert_with(|| json!([]));
    if !deny.is_array() {
        *deny = json!([]);
    }
    let deny = deny.as_array_mut().expect("deny is an array");
    let wanted = BUILTIN_TOOLS.iter().map(|tool| (*tool).to_owned()).chain(
        deny_mcp_servers
            .iter()
            .map(|server| format!("mcp__{server}")),
    );
    for rule in wanted {
        if !deny.iter().any(|existing| existing.as_str() == Some(&rule)) {
            deny.push(Value::String(rule));
        }
    }
    Value::Object(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extends_existing_deny_list_without_duplicates() {
        let base = json!({
            "model": "opus",
            "permissions": { "allow": ["Bash(git:*)"], "deny": ["Bash", "Read"] },
        });
        let settings = deny_all_but_own_tools(&base, &["rho"]);
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["permissions"]["allow"], json!(["Bash(git:*)"]));
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny[0], "Bash");
        assert_eq!(deny[1], "Read");
        assert_eq!(
            deny.iter().filter(|rule| *rule == "Bash").count(),
            1,
            "existing rules are not repeated"
        );
        assert_eq!(deny.len(), BUILTIN_TOOLS.len() + 1);
        assert!(deny.contains(&json!("mcp__rho")));
        assert!(deny.contains(&json!("ToolSearch")));
    }

    #[test]
    fn replaces_malformed_sections() {
        let settings = deny_all_but_own_tools(&json!({"permissions": "no"}), &[]);
        assert_eq!(
            settings["permissions"]["deny"].as_array().unwrap().len(),
            BUILTIN_TOOLS.len()
        );
        let settings = deny_all_but_own_tools(&json!([1]), &[]);
        assert!(settings["permissions"]["deny"].is_array());
    }
}
