use std::sync::Arc;

use crate::db::{AgentRole, AgentSpawnedBy};
use crate::multi_agent_tools::MultiAgentTools;

#[path = "prompt/advisor-high.rs"]
mod advisor_high;
#[path = "prompt/eng-high.rs"]
mod eng_high;

use advisor_high::ADVISOR_BASE_PROMPT;
use eng_high::BASE_PROMPT;

/// `multi_agent` is set for pooled agents, which get the multi-agent tools and
/// the section explaining them. `code_mode` is set when the agent's tool
/// surface uses the selected code-mode runtime.
pub fn prompt(
    view: &rho_workspaces::View,
    multi_agent: Option<&MultiAgentTools>,
    code_mode: Option<rho_agent_tools::CodeMode>,
    role: AgentRole,
    host_specs: &[rho_core::ToolSpec],
) -> Arc<str> {
    let entries = view.entries();
    let workdirs = entries
        .iter()
        .map(|workspace| WorkdirPrompt {
            path: workspace.repo().to_string(),
            kind: WorkdirKind::of(workspace),
        })
        .collect::<Vec<_>>();
    let (agents_md, skills) = {
        let (agents_files, skills) = merged_context(entries);
        let skills = skills
            .into_iter()
            .filter(|skill| role.is_engineer() || skill.name != "delegate-engineering")
            .collect::<Vec<_>>();
        (
            render_agents_md_prompt(&agents_files).unwrap_or_default(),
            render_skills_prompt(&skills).unwrap_or_default(),
        )
    };
    let team_context = multi_agent.map_or_else(String::new, |tools| {
        let agent_id = tools.display_id(tools.self_id());
        let identity = match tools.parent() {
            Some(parent) => format!(
                "You are an agent in a team of agents collaborating to complete a task. Your \
                 agent id is {agent_id}; your parent agent is {}.\n\nMessages from your \
                 parent define your task. When you provide a final response, that content is \
                 mailed back to your parent automatically.",
                tools.display_id(parent)
            ),
            None => format!(
                "You are the primary agent in a team of agents collaborating to fulfill the \
                 user's goals. Your agent id is {agent_id}.\n\nAt the start of your turn, you \
                 are the active agent."
            ),
        };
        if matches!(role, AgentRole::Advisor { .. }) {
            return format!(
                "## Team Context

{identity}

Complete your independent analysis and return it to your parent through your \
final response. Use the available messaging tool to request context from a known \
agent and the idle mechanism described above when blocked on a reply.
"
            );
        }
        let ownership = match tools.spawned_by() {
            AgentSpawnedBy::Direct => {
                "You were started directly and own the user's technical outcome."
            }
            AgentSpawnedBy::Engineer => {
                "You were spawned by another Engineer. Own the bounded assignment in the \
                 parent message; your final response is mailed to that Engineer."
            }
        };
        let message_tool = match code_mode {
            Some(rho_agent_tools::CodeMode::Python) => "agents.message",
            Some(rho_agent_tools::CodeMode::JavaScript) => "tools.message_agent",
            None => "message_agent",
        };
        format!(
            "## Team Context

{identity}

{ownership}

You will receive agent messages in this format:
```
Message Type: MESSAGE
Sender: <agent id>
Payload:
<payload text>
```

Use `{message_tool}` for bidirectional communication with any known agent. Mail \
does not interrupt an in-flight request, but it can start or continue your next \
request.

"
        )
    });
    let tool_results = if code_mode == Some(rho_agent_tools::CodeMode::Python) {
        ""
    } else {
        TOOL_RESULTS_PROMPT
    };
    let code_mode = match code_mode {
        Some(rho_agent_tools::CodeMode::Python) => rho_agent_tools::python_instructions(host_specs),
        Some(rho_agent_tools::CodeMode::JavaScript) => JAVASCRIPT_CODE_MODE_PROMPT.to_owned(),
        None => String::new(),
    };
    let role_prompt = match role {
        AgentRole::Engineer { .. } | AgentRole::Advisor { .. } => "",
    };
    let base_prompt = if matches!(role, AgentRole::Advisor { .. }) {
        ADVISOR_BASE_PROMPT
    } else {
        BASE_PROMPT
    };
    let environment = render_environment_prompt(&workdirs);
    let workspace = render_workspace_prompt(&workdirs);
    format!("{base_prompt}{agents_md}{skills}{code_mode}{tool_results}{team_context}{role_prompt}{workspace}{environment}")
        .into()
}

pub fn claude_prompt(
    view: Option<&rho_workspaces::View>,
    multi_agent: Option<&MultiAgentTools>,
    role: AgentRole,
) -> Arc<str> {
    let team = multi_agent.map_or_else(String::new, |tools| {
        let identity = match tools.parent() {
            Some(parent) => format!(
                "Your Rho agent id is {}; your parent agent is {}. Your final response is mailed \
                 to your parent automatically.",
                tools.display_id(tools.self_id()),
                tools.display_id(parent),
            ),
            None => format!(
                "You are the primary Rho agent. Your agent id is {}.",
                tools.display_id(tools.self_id())
            ),
        };
        format!("## Rho Team Context\n\n{identity}\n\n")
    });
    let role_prompt = match role {
        AgentRole::Engineer { .. } => "",
        AgentRole::Advisor { .. } => ADVISOR_PROMPT,
    };
    let workspace = view
        .filter(|_| role.is_engineer() || matches!(role, AgentRole::Advisor { .. }))
        .map_or_else(String::new, |view| {
            let workdirs = view
                .entries()
                .iter()
                .map(|workspace| WorkdirPrompt {
                    path: workspace.repo().to_string(),
                    kind: WorkdirKind::of(workspace),
                })
                .collect::<Vec<_>>();
            render_workspace_prompt(&workdirs)
        });
    format!("{team}{role_prompt}{workspace}").into()
}

/// One workdir as the prompt renders it: the agent-visible path and the kind
/// of checkout mounted there.
struct WorkdirPrompt {
    path: String,
    kind: WorkdirKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkdirKind {
    Live,
    Managed,
    Sandbox,
}

impl WorkdirKind {
    fn of(workspace: &rho_workspaces::Workspace) -> Self {
        if workspace.is_sandbox() {
            Self::Sandbox
        } else if workspace.is_user_checkout() {
            Self::Live
        } else {
            Self::Managed
        }
    }
}

/// Union of every workdir's discovered context: AGENTS.md files deduped by
/// path (the user-level file appears in each entry's discovery), skills
/// deduped by name with earlier (primary-first) workdirs winning.
fn merged_context(
    entries: &[Arc<rho_workspaces::Workspace>],
) -> (
    Vec<rho_context_config::AgentsFile>,
    Vec<rho_context_config::Skill>,
) {
    let mut seen_files = std::collections::HashSet::new();
    let mut seen_skills = std::collections::HashSet::new();
    let mut agents_files = Vec::new();
    let mut skills = Vec::new();
    for entry in entries {
        let context = entry.discovered_context();
        for diagnostic in &context.diagnostics {
            eprintln!(
                "rho-agent: context config {:?}: {}: {}",
                diagnostic.kind,
                diagnostic.path.display(),
                diagnostic.message
            );
        }
        for file in &context.agents_files {
            if seen_files.insert(file.file_path.clone()) {
                agents_files.push(file.clone());
            }
        }
        for skill in &context.skills {
            if seen_skills.insert(skill.name.clone()) {
                skills.push(skill.clone());
            }
        }
    }
    (agents_files, skills)
}

const ADVISOR_PROMPT: &str = "## Advisor

You are an independent technical second opinion. Analyze the question deeply, \
surface risks and tradeoffs, and recommend a path. You are advisory only: do \
not implement changes.

";

const JAVASCRIPT_CODE_MODE_PROMPT: &str = "## JavaScript Code Mode

`exec` runs JavaScript in a persistent REPL with top-level await. Call tools through
`tools.NAME(...)`; use `text(value)` to display results and `image(item)` for images.
Batch independent calls with `Promise.all`. See exec for the runtime API and schemas.
Use the separate `wait` tool when there is nothing else to do.

";

const TOOL_RESULTS_PROMPT: &str = "## How tool results arrive

Every tool call is answered with its finished result, however long it takes; you never \
poll. A command that is still running when something else needs your attention is \
answered with what it has printed so far and a session ID, and everything it prints later \
arrives on that same call by itself. If you have nothing to do until something happens, \
call `wait` with the number of seconds you can afford to be left alone: anything ending, a \
user message or mail wakes you sooner, so a long interval costs nothing and a short one \
costs a request.

";

fn render_agents_md_prompt(files: &[rho_context_config::AgentsFile]) -> Option<String> {
    if files.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("## AGENTS.md instructions\n");
    out.push_str("The following instructions were loaded from AGENTS.md files. They are user/project instructions: follow them unless they conflict with higher-priority system or developer instructions. More specific files appear later and usually override broader ones.\n\n");
    for file in files {
        out.push_str("<AGENTS_FILE path=\"");
        out.push_str(file.file_path.as_str());
        out.push_str("\">\n");
        out.push_str(&file.content);
        if !file.content.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</AGENTS_FILE>\n\n");
    }
    Some(out)
}

fn render_skills_prompt(skills: &[rho_context_config::Skill]) -> Option<String> {
    let mut skills = skills.iter().collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    if skills.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("## Skills\n");
    out.push_str("In your workspace you have skills the user created. A **skill** is a guide for proven techniques, patterns, or tools. If a skill exists for a task, you must do it. The following skills provide specialized instructions for specific tasks.\n");
    out.push_str("### Available skills\n");
    for skill in skills {
        out.push_str("- ");
        out.push_str(&skill.name);
        out.push_str(": ");
        out.push_str(&skill.description);
        out.push_str(" (file: ");
        out.push_str(skill.file_path.as_str());
        out.push_str(")\n");
    }
    out.push_str("\n### How to use skills\n");
    out.push_str("- Discovery: The list above is the skills available in this session (name + description + file path). Skill bodies live on disk at the listed paths. Read the listed file path before using a skill; do not assume the description is enough.\n");
    out.push_str("- Trigger rules: If the user names a skill (with `$SkillName` or plain text) OR the task clearly matches a skill's description shown above, you must use that skill for that turn. Multiple mentions mean use them all. Do not carry skills across turns unless re-mentioned.\n");
    out.push_str("- Missing/blocked: If a named skill isn't in the list or the path can't be read, say so briefly and continue with the best fallback.\n");
    out.push_str("- How to use a skill (progressive disclosure):\n");
    out.push_str("  1) After deciding to use a skill, open and read its SKILL.md file before taking task actions.\n");
    out.push_str("  2) When `SKILL.md` references relative paths (e.g., `scripts/foo.py`), resolve them relative to the skill directory listed above first.\n");
    out.push_str("  3) If `SKILL.md` points to extra folders such as `references/`, load only the specific files needed for the request; don't bulk-load everything.\n");
    out.push_str("  4) If `scripts/` exist, prefer running or patching them instead of retyping large code blocks.\n");
    out.push_str(
        "  5) If `assets/` or templates exist, reuse them instead of recreating from scratch.\n",
    );
    out.push_str("- Context hygiene:\n");
    out.push_str("  - Keep context small: summarize long sections instead of pasting them; only load extra files when needed.\n");
    out.push_str("  - Avoid deep reference-chasing: prefer opening only files directly linked from `SKILL.md` unless you're blocked.\n");
    out.push_str("- Safety and fallback: If a skill can't be applied cleanly (missing files, unclear instructions), state the issue, pick the next-best approach, and continue.\n");
    out.push('\n');
    Some(out)
}

fn render_environment_prompt(workdirs: &[WorkdirPrompt]) -> String {
    let working_directory = &workdirs[0].path;
    let mut out = format!(
        "## Environment

Working directory: {working_directory}

Relative paths in commands and patches resolve against this directory.
"
    );
    if workdirs.len() > 1 {
        out.push_str("\nAdditional workdirs in your working set:\n");
        for workdir in &workdirs[1..] {
            let binding = match workdir.kind {
                WorkdirKind::Managed => "a Rho-managed jj workspace",
                WorkdirKind::Sandbox => "a Rho-managed sandbox workspace",
                WorkdirKind::Live => "a live directory rather than a Rho-managed workspace",
            };
            out.push_str(&format!("- {} ({binding})\n", workdir.path));
        }
        out.push_str("\nStay within these directories unless the user points you elsewhere.\n");
    } else {
        out.push_str("Stay within it unless the user points you elsewhere.\n");
    }
    out
}

/// Draft replacement for the rendered `## Workspace Context` section under the
/// per-agent clone model (each agent gets its own jj repo over shared storage
/// instead of a Rho-managed workspace in one shared repo). Unused until that
/// runtime lands.
///
/// Variants the renderer still needs: "Every repository in your working set is
/// your own clone" for multiple jj workdirs; a live-directory line for plain
/// workdirs; the existing per-workdir list when the working set is mixed; and,
/// for an agent that joined its spawner's checkout, "You share this clone with
/// the agent that started you, so your edits are visible to it immediately" in
/// place of the second sentence.
#[allow(dead_code)]
const DRAFT_CLONE_WORKSPACE_PROMPT: &str = "## Workspace Context

Your working directory is your own clone of the repository. No other agent \
works in it, so edit files and run tests here freely. The user may also open \
and edit it — treat changes you did not make as intentional and leave them \
alone.

";

/// Draft version-control section to accompany [`DRAFT_CLONE_WORKSPACE_PROMPT`],
/// rendered only when at least one workdir is a jj repo. Landing and history
/// editing are deliberately absent: the `land` skill owns that policy, and
/// restating it here is the repetition that makes agents ask before safe,
/// expected actions.
///
/// Blocked on the handoff fix: `delegate-engineering/SKILL.md` and the
/// `spawn_engineer` result still hand out `jj diff -r '<workspace>@'`, which
/// reads empty once an agent commits. That needs to become a range from the
/// spawn base, which is also correct when the agent leaves work uncommitted.
#[allow(dead_code)]
const DRAFT_JJ_WORKFLOW_PROMPT: &str = "## jj Workflow

This repository uses jj. Record a change once it is complete: \
`jj commit -m '<message>'` for new work, or `jj squash -u` to fold a follow-up \
into the change you just made. Work still in progress can stay in the working \
copy.

";

fn render_workspace_prompt(workdirs: &[WorkdirPrompt]) -> String {
    let managed = workdirs
        .iter()
        .filter(|workdir| workdir.kind == WorkdirKind::Managed)
        .count();
    let sandboxed = workdirs
        .iter()
        .filter(|workdir| workdir.kind == WorkdirKind::Sandbox)
        .count();
    let mut out = String::from("## Workspace Context\n\n");
    if managed == workdirs.len() {
        if workdirs.len() == 1 {
            out.push_str("Your working directory is a Rho-managed jj workspace.\n\n");
        } else {
            out.push_str(
                "Every repository workdir in your working set is a Rho-managed jj workspace.\n\n",
            );
        }
    } else if sandboxed == workdirs.len() {
        if workdirs.len() == 1 {
            out.push_str("Your working directory is a Rho-managed sandbox workspace.\n\n");
        } else {
            out.push_str(
                "Every workdir in your working set is a Rho-managed sandbox workspace.\n\n",
            );
        }
    } else if managed == 0 && sandboxed == 0 {
        out.push_str(
            "Your workdirs are live directories rather than Rho-managed jj workspaces. Edits there are immediately visible to other processes using those directories.\n\n",
        );
    } else {
        out.push_str("Workspace management differs across your working set:\n");
        for workdir in workdirs {
            let management = match workdir.kind {
                WorkdirKind::Managed => "Rho-managed jj workspace",
                WorkdirKind::Sandbox => "Rho-managed sandbox workspace",
                WorkdirKind::Live => "live directory",
            };
            out.push_str(&format!("- {} — {management}\n", workdir.path));
        }
        out.push('\n');
    }
    if managed > 0 {
        out.push_str("Each Rho-managed jj workdir is a workspace: the checkout you are working in, with a working-copy commit named `@`. jj records your edits into `@` as you work, so keeping them takes no extra step. Files and uncommitted changes already present are the starting state you were given, not leftovers to clean up. Other workspaces have their own working-copy commits; leave commits you did not create alone unless the task is to work on them.\n\n");
    }
    if sandboxed > 0 {
        out.push_str("A Rho-managed sandbox workspace masks the repository's original VCS metadata from commands and presents a separate synthetic Git baseline. Work with the checkout and VCS view provided inside the sandbox rather than assuming the origin checkout's metadata is available.\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use rho_context_config::{AgentsFile, Skill};

    use super::*;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            file_path: Utf8PathBuf::from(format!("/repo/.agents/skills/{name}/SKILL.md")),
        }
    }

    fn agents_file(path: &str, content: &str) -> AgentsFile {
        AgentsFile {
            file_path: Utf8PathBuf::from(path),
            content: content.to_owned(),
        }
    }

    #[test]
    fn renders_big_skill_guidance_with_file_paths() {
        let prompt = render_skills_prompt(&[skill("demo", "Demo skill")]).unwrap();
        assert!(prompt.contains("## Skills"));
        assert!(prompt.contains("If a skill exists for a task, you must do it"));
        assert!(prompt.contains("- demo: Demo skill (file: /repo/.agents/skills/demo/SKILL.md)"));
        assert!(prompt.contains("open and read its SKILL.md file"));
    }

    #[test]
    fn renders_agents_md_guidance_with_file_boundaries() {
        let prompt =
            render_agents_md_prompt(&[agents_file("/repo/AGENTS.md", "Read the docs.")]).unwrap();
        assert!(prompt.contains("## AGENTS.md instructions"));
        assert!(
            prompt
                .contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">\nRead the docs.\n</AGENTS_FILE>")
        );
        assert!(prompt.contains("follow them unless they conflict"));
    }

    fn workdir(path: &str, kind: WorkdirKind) -> WorkdirPrompt {
        WorkdirPrompt {
            path: path.to_owned(),
            kind,
        }
    }

    #[test]
    fn managed_workspace_prompt_is_informational() {
        let prompt = render_workspace_prompt(&[workdir("/repo", WorkdirKind::Managed)]);
        assert!(prompt.contains("## Workspace Context"));
        assert!(prompt.contains("working directory is a Rho-managed jj workspace"));
        assert!(prompt.contains("working-copy commit named `@`"));
        assert!(prompt.contains("keeping them takes no extra step"));
        assert!(prompt.contains("starting state you were given"));
        assert!(prompt.contains("leave commits you did not create alone"));
        assert!(!prompt.contains("Agent views"));
        assert!(!prompt.contains("user's own checkout"));
        assert!(!prompt.contains("working in place"));
        assert!(!prompt.contains("Delegated Engineer Isolation"));
        assert!(!prompt.contains("do not create"));
    }

    #[test]
    fn live_workspace_prompt_reports_management() {
        let prompt = render_workspace_prompt(&[workdir("/repo", WorkdirKind::Live)]);
        assert!(prompt.contains("live directories rather than Rho-managed jj workspaces"));
        assert!(prompt.contains("immediately visible to other processes"));
    }

    #[test]
    fn sandbox_workspace_prompt_does_not_call_it_live() {
        let prompt = render_workspace_prompt(&[workdir("/repo", WorkdirKind::Sandbox)]);
        assert!(prompt.contains("Rho-managed sandbox workspace"));
        assert!(prompt.contains("masks the repository's original VCS metadata"));
        assert!(!prompt.contains("immediately visible to other processes"));
    }

    #[test]
    fn workspace_prompt_lists_mixed_workdirs() {
        let prompt = render_workspace_prompt(&[
            workdir("/repo", WorkdirKind::Managed),
            workdir("/docs", WorkdirKind::Live),
        ]);
        assert!(prompt.contains("Workspace management differs across your working set"));
        assert!(prompt.contains("- /repo — Rho-managed jj workspace"));
        assert!(prompt.contains("- /docs — live directory"));
    }

    #[test]
    fn role_guidance_is_separate_from_the_base_prompt() {
        assert!(!BASE_PROMPT.contains("## Advisor"));
        assert!(ADVISOR_PROMPT.contains("advisory only"));
        for section in [
            "## Read before you advise",
            "## Work quickly",
            "## Review stance",
            "## Engineering judgment",
            "## Debugging",
            "## Advisory mode",
            "## Tool use",
            "## Response format",
            "## Communication",
        ] {
            assert!(ADVISOR_BASE_PROMPT.contains(section), "missing {section}");
        }
        assert!(ADVISOR_BASE_PROMPT.contains("Do not rerun tests, builds, or checks"));
        assert!(ADVISOR_BASE_PROMPT.contains("narrowly scoped scratch edit"));
        assert!(!ADVISOR_BASE_PROMPT.contains("zero-shot"));
        assert!(!ADVISOR_BASE_PROMPT.contains("one-shot"));
        assert!(!ADVISOR_BASE_PROMPT.contains("Only your last message"));
        assert!(!ADVISOR_BASE_PROMPT.contains("`finder`"));
        assert!(!ADVISOR_BASE_PROMPT.contains("`librarian`"));
    }

    #[test]
    fn environment_prompt_mentions_working_directory() {
        let prompt = render_environment_prompt(&[workdir("/repo", WorkdirKind::Managed)]);
        assert!(prompt.contains("Working directory: /repo"));
        assert!(!prompt.contains("jj workspace id"));
        assert!(!prompt.contains("Additional workdirs"));
    }

    #[test]
    fn environment_prompt_lists_additional_workdirs() {
        let prompt = render_environment_prompt(&[
            workdir("/repo", WorkdirKind::Managed),
            workdir("/lib", WorkdirKind::Managed),
            workdir("/docs", WorkdirKind::Live),
        ]);
        assert!(prompt.contains("Working directory: /repo"));
        assert!(prompt.contains("- /lib (a Rho-managed jj workspace)"));
        assert!(prompt.contains("- /docs (a live directory"));
    }
}
