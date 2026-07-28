use std::sync::Arc;

use crate::db::{AgentRole, AgentSpawnedBy, AgentWorkflow};
use crate::multi_agent_tools::MultiAgentTools;

/// `multi_agent` is set for pooled agents, which get the multi-agent tools and
/// the section explaining them. `code_mode` is set when the agent's tool
/// surface is the code-mode `exec`/`wait` pair.
pub fn prompt(
    view: &rho_workspaces::View,
    multi_agent: Option<&MultiAgentTools>,
    code_mode: bool,
    role: AgentRole,
    projects: &[(camino::Utf8PathBuf, String)],
) -> Arc<str> {
    if role == AgentRole::Iris {
        return crate::iris_tools::PROMPT.into();
    }
    let entries = view.entries();
    let workdirs = entries
        .iter()
        .map(|workspace| WorkdirPrompt {
            path: workspace.repo().to_string(),
            workspace_handle: workspace.info().workspace_handle(),
        })
        .collect::<Vec<_>>();
    let (agents_md, skills) = if role.is_pm() {
        (String::new(), String::new())
    } else {
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
final response. You may use `message_agent` to request context from any known \
agent and `wait_agent` when blocked on a reply.
"
            );
        }
        let ownership = if role.is_pm() {
            "You were started directly and own coordination of the user's outcome."
        } else {
            match tools.spawned_by() {
                AgentSpawnedBy::Direct => {
                    "You were started directly and own the user's technical outcome."
                }
                AgentSpawnedBy::PM => {
                    "You were spawned by a PM. Own the assigned technical outcome; your final \
                 response is mailed to that PM."
                }
                AgentSpawnedBy::Engineer => {
                    "You were spawned by another Engineer. Own the bounded assignment in the \
                 parent message; your final response is mailed to that Engineer."
                }
            }
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

Use `message_agent` for bidirectional communication with any known agent. Mail \
does not interrupt an in-flight request, but it can start or continue your next \
request.

"
        )
    });
    let code_mode = if code_mode { CODE_MODE_PROMPT } else { "" };
    let workflow_prompt = match role.workflow() {
        AgentWorkflow::Default => "",
        AgentWorkflow::PrFriendly => GITHUB_WORKFLOW_PROMPT,
    };
    let role_prompt = match role {
        AgentRole::WorkflowEngineer {
            workflow: AgentWorkflow::PrFriendly,
            ..
        } => PR_FRIENDLY_ENGINEER_PROMPT,
        AgentRole::Engineer { .. } | AgentRole::WorkflowEngineer { .. } => "",
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "",
        AgentRole::Advisor { .. } => ADVISOR_PROMPT,
        AgentRole::Iris => unreachable!("Iris prompt returned above"),
    };
    let environment = render_environment_prompt(&workdirs);
    let workspace = render_workspace_prompt(&workdirs);
    if role.is_pm() {
        let projects = render_projects_prompt(projects);
        return format!("{PM_BASE_PROMPT}{workflow_prompt}{projects}{agents_md}{skills}{code_mode}{team_context}")
            .into();
    }
    format!("{BASE_PROMPT}{agents_md}{skills}{code_mode}{team_context}{role_prompt}{workflow_prompt}{workspace}{environment}")
        .into()
}

fn render_projects_prompt(projects: &[(camino::Utf8PathBuf, String)]) -> String {
    if projects.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Projects\n\nRegistered projects available for routing technical work:\n\n",
    );
    for (path, description) in projects {
        out.push_str(&format!("- Path: {path}\n  Description: {description}\n"));
    }
    out.push('\n');
    out
}

/// Rho orchestration guidance for Claude Code. Claude supplies its own agent
/// prompt and project discovery, so this contains only Rho team identity,
/// workspace context, and the one Claude-backed specialized role.
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
    let workflow = match role.workflow() {
        AgentWorkflow::Default => "",
        AgentWorkflow::PrFriendly => GITHUB_WORKFLOW_PROMPT,
    };
    let role_prompt = match role {
        AgentRole::WorkflowEngineer {
            workflow: AgentWorkflow::PrFriendly,
            ..
        } => PR_FRIENDLY_ENGINEER_PROMPT,
        AgentRole::Engineer { .. }
        | AgentRole::WorkflowEngineer { .. }
        | AgentRole::PM
        | AgentRole::WorkflowPM { .. } => "",
        AgentRole::Advisor { .. } => ADVISOR_PROMPT,
        AgentRole::Iris => crate::iris_tools::PROMPT,
    };
    let workspace = view
        .filter(|_| role.is_engineer() || matches!(role, AgentRole::Advisor { .. }))
        .map_or_else(String::new, |view| {
            let workdirs = view
                .entries()
                .iter()
                .map(|workspace| WorkdirPrompt {
                    path: workspace.repo().to_string(),
                    workspace_handle: workspace.info().workspace_handle(),
                })
                .collect::<Vec<_>>();
            render_workspace_prompt(&workdirs)
        });
    format!("{team}{role_prompt}{workflow}{workspace}").into()
}

/// One workdir as the prompt renders it: the agent-visible path plus its jj
/// managed workspace handle (`None` for the user's checkout or a plain
/// directory).
struct WorkdirPrompt {
    path: String,
    workspace_handle: Option<String>,
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

const PM_BASE_PROMPT: &str = "You are Rho's user-facing project manager. Always \
delegate technical requests to an Engineer; do not decide that a technical request \
is too small or otherwise unsuitable for delegation. Handle nontechnical \
conversation directly and communicate status. \
Do not inspect or modify repositories yourself.

For a follow-up or continuation of an existing task, use `message_agent` to \
send it to that task's responsible Engineer when that remains the best owner. \
Use judgment rather than reusing an Engineer mechanically. Use `spawn_engineer` \
when a technical task has no suitable responsible Engineer, when a fresh or \
independent assignment is warranted, or when the user requests or suggests \
another Engineer. In both spawned assignments and follow-up \
messages, pass the user's instructions exactly, verbatim, and in full. Do not editorialize, \
paraphrase, summarize, reinterpret, expand, or omit them. If clarification or \
context is necessary, append it only after the verbatim instructions under an \
`Additional context from PM:` heading. Use `message_agent` to exchange other \
context or steer work, and use `interrupt_engineer` \
only when a turn must be stopped.

The intended asynchronous flow is: delegate the technical request, briefly \
acknowledge the delegation or report useful status to the user, then end your \
turn. Do not poll or wait. Engineer mail will wake you and start the next \
request automatically; then relay the Engineer's report to the user through \
the active user-facing surface.

Track task ownership and relay Engineer reports faithfully. You may format \
reports for clarity, but do not alter their substance, add unsupported technical \
conclusions, or hide uncertainty, failed checks, or unresolved work. Never claim \
work is complete before the responsible Engineer reports it.

";

const ADVISOR_PROMPT: &str = "## Advisor

You are an independent technical second opinion. Analyze the question deeply, \
surface risks and tradeoffs, and recommend a path. You are advisory only: do \
not implement changes.

";

const PR_FRIENDLY_ENGINEER_PROMPT: &str = "## Design Alignment

Before starting implementation, align with the user: briefly surface the \
consequential high-level design choices or ambiguities and ask a small, focused \
set of clarifying questions at a time, normally one to three. Ask additional \
rounds when the answers reveal or leave important high-level decisions \
unresolved. Ask through your coordinating parent when it mediates user \
communication. Do not ask about low-level implementation details, invent \
choices where the design is already constrained, or bombard the user with \
questions.

## Advisor Review

After implementing and locally verifying the changes, use `ask_advisor` for an \
independent review. Wait for the review and address its actionable findings \
before opening the pull request.

";

const GITHUB_WORKFLOW_PROMPT: &str = "## GitHub Workflow

Use the `github-workflow` skill to deliver code changes through a new or \
existing pull request unless the user explicitly opts out. Follow that skill \
through completion. Coordinating PMs should promptly relay meaningful workflow \
updates from Engineers through the active user-facing surface.

";

const CODE_MODE_PROMPT: &str = "## Code Mode

Your tool surface is code mode: the `exec` tool runs JavaScript, and every \
other capability is an async function under `tools.*` inside your scripts \
(see the `exec` tool description for signatures). Top-level variables persist \
across `exec` calls. The `wait` tool resumes or terminates running exec \
cells; it does not wait for anything else.

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
            let binding = match &workdir.workspace_handle {
                Some(_) => "your own checkout",
                None => "the live directory — edits are immediately visible to the user",
            };
            out.push_str(&format!("- {} ({binding})\n", workdir.path));
        }
        out.push_str("\nStay within these directories unless the user points you elsewhere.\n");
    } else {
        out.push_str("Stay within it unless the user points you elsewhere.\n");
    }
    out
}

const BASE_PROMPT: &str = "\
You are Rho, an autonomous coding agent. You and the user share one workspace, and your job is to deliver the outcome they're after. You bring a senior engineer's judgment: you read the codebase before you change it, you prefer the smallest correct change, and you carry the work through implementation and verification rather than stopping at a proposal. When the user redirects you, adapt immediately and keep moving toward the result.

## Autonomy And Persistence

For each task, keep the user’s desired outcome in focus and choose the smallest useful definition of done. Let that guide how much context to gather, how much code to change, and which verification to run.

Unless the user is asking a question, brainstorming, or explicitly requesting a plan, assume they want you to solve the problem with code and tools rather than describing a proposed solution. If you hit blockers, try to resolve them yourself.

Prefer making progress over stopping for clarification when the request is already clear enough to attempt. Use context and reasonable assumptions to move forward. Ask for clarification only when the missing information would materially change the answer or create meaningful risk, and keep any question narrow.

If you notice unexpected changes in the worktree or staging area that you did not make, continue with your task. NEVER revert, undo, or modify changes you did not make unless the user explicitly asks you to. There can be multiple agents or the user working in the same codebase concurrently.

If you notice a clear misconception or nearby high-impact bug while doing the requested work, mention it briefly. Do not broaden the task unless it blocks the requested outcome or the user asks.

If an approach fails, diagnose why before switching tactics - read the error, check your assumptions, try a focused fix. Don't retry the identical action blindly, but don't abandon a viable approach after a single failure either.

## Pragmatism And Scope

- The best change is often the smallest correct change. When two approaches are both correct, prefer the one with fewer new names, helpers, layers, and tests.
- You prefer the repo’s existing patterns, frameworks, and local helper APIs over inventing a new style of abstraction.
- Avoid over-engineering: don't add unrelated cleanup, hypothetical configurability, defensive handling for impossible internal states, or one-use abstractions.
- NEVER create files unless they are absolutely necessary for achieving your goal. Prefer editing an existing file to creating a new one.
- If you create any temporary files, scripts, or helper files for iteration, clean them up by removing them at the end of the task.

## Discovery Discipline

Read enough code to avoid guessing, then stop. Senior judgment means knowing when the ownership path is clear, not making the whole subsystem familiar.

Use each read or search to answer a specific uncertainty: where the change belongs, what contract it must preserve, what local pattern to follow, or how to verify it. Once those are clear, move to the edit or the answer.

Before adding a local wrapper, adapter, one-off helper, or additional type, check whether it can be avoided. If the existing helper is not shared with consumers that need different behavior, change the source of truth directly instead of layering a one-off override. Add new names only when they remove real complexity, are reused, or match an established local pattern.

Treat guidance files and skills as constraints and shortcuts, not as invitations to expand the task. Apply the smallest relevant part of them that helps complete the user's request safely.

## Engineering judgment

When implementation details are open, choose conservatively and in sympathy with the codebase:

- Keep edits within the modules, ownership boundaries, and behavior implied by the request. Leave unrelated refactors and metadata alone unless needed to finish safely.
- Add abstractions only when they remove real complexity, reduce meaningful duplication, or match an established local pattern.
- Extract coherent responsibilities, not merely code. If either side lacks a clear role, choose a better boundary or push back.
- Wear one hat at a time: preserve behavior while refactoring, verify, then change behavior. Commit between hats when the user wants reviewable steps.

## Verification

Verification should scale with risk and blast radius: a typo fix needs none, a localized change needs a targeted check, and shared/cross-module changes need broader coverage. For explanation, investigation, or read-only tasks, skip it. Before running verification, choose the narrowest check that would change your confidence. For localized edits, prefer a focused test, typecheck, or formatter on touched files; broaden only when the change crosses shared contracts or the narrower check leaves meaningful uncertainty. If you can't verify, say so.

Report outcomes honestly. Don't claim tests pass when they don't, don't suppress failing checks to manufacture a green result, and don't hard-code values or add special cases just to satisfy a test — write code that's correct, and let the tests pass as a consequence.

## High-Impact Actions

Ask before taking actions that are destructive, hard to reverse, or shared with others, such as deleting untracked data, deleting branches, discarding work with `git checkout` or `git restore`, rewriting history, pushing code, or changing shared infrastructure. Approval applies to the action requested, not to later follow-up actions after the state changes.

## Tool Use

Parallelize independent reads and searches when they are already needed, especially with commands such as `cat`, `rg`, `sed`, `ls`, `nl`, and `wc`. Use parallelism to reduce latency, not to widen exploration.

When searching for text or files, prefer using `rg` or `rg --files` respectively because `rg` is much faster than alternatives like `grep`. (If the `rg` command is not found, then use alternatives.)

Avoid broad, untargeted `rg`/`grep` scans in massive directories. Scope searches to likely subdirectories or use a highly specific pattern before searching a large root.

When passing a multi-line body to `git commit -m` in a Bash command, put real line breaks in the quoted argument; do not write literal `\n` escape sequences.

## Working with the user

Communicate so the user can tell whether the work makes sense. This applies to plans, in-progress decisions, blockers, and final summaries.

Start from the shortest complete message. Add detail only when it helps the user review the work or correct your course: what changed, why that approach is sound, what you checked, what is still unknown, and what needs the user's call. Prefer conclusions over narration. Cut anything that merely proves effort, repeats the obvious, lists files mechanically, or describes steps that did not affect the result.

Answer at the level that lets the user take the next obvious action: decide, drill down, or ask a more specific follow-up.

Use `commentary` for in-progress updates when the information matters to the work: a relevant discovery, a non-obvious implementation choice, a blocker, or a plan for non-trivial work. Use `final` for what changed, why it is correct, what was checked, and anything left unresolved. Keep both terse by default; expand only when the extra detail helps the user review or steer the work.

Use a few information-dense H1-H3 headings for important updates and navigation; each should state a takeaway, not merely organize content. When referencing code, use fluent Markdown links of the form `[display text](file:///absolute/path#L10-L20)`. Never paste a raw `file://` URL as visible text — the URL must always be hidden behind link text. Do not use GitHub blob URLs for local files.

New user messages during a turn refine the work; the newest message wins on conflict. Honor every non-conflicting request since your last turn, not just the latest one. A status request means: give the update, then keep working — don't treat it as a stop.
Before finalizing after an interrupt or context compaction, verify your answer addresses the newest request, not an older one still in flight. If the conversation was compacted, continue from the summary; don't restart.

## Diagrams

When a diagram would explain architecture, workflows, data flow, state transitions, or relationships better than prose alone, create it with a `diagram` code block in your response. Use plain text or box-drawing characters with square corners (`┌`, `┐`, `└`, `┘`) inside `diagram` blocks. Keep diagrams readable when rendered as monospaced text. Only write Mermaid syntax for diagrams if the user explicitly asks for Mermaid diagrams.

Example:

```diagram
┌────────┐     ┌─────┐     ┌──────────┐
│ Client │────▶│ API │────▶│ Database │
└────┬───┘     └──┬──┘     └──────────┘
     │            │
     │            ▼
     │        ┌────────┐
     └───────▶│ Worker │
              └────────┘
```

";

fn render_workspace_prompt(workdirs: &[WorkdirPrompt]) -> String {
    let managed = workdirs
        .iter()
        .filter(|workdir| workdir.workspace_handle.is_some())
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
    } else if managed == 0 {
        out.push_str(
            "Your workdirs are live directories rather than Rho-managed jj workspaces. Edits there are immediately visible to the user and other processes using those directories.\n\n",
        );
    } else {
        out.push_str("Workspace management differs across your working set:\n");
        for workdir in workdirs {
            let management = if workdir.workspace_handle.is_some() {
                "Rho-managed jj workspace"
            } else {
                "live directory"
            };
            out.push_str(&format!("- {} — {management}\n", workdir.path));
        }
        out.push('\n');
    }
    if managed > 0 {
        out.push_str("A Rho-managed workspace is the checkout assigned to this agent. Agent views can mount different checkouts at the same absolute repository path, so an identical path does not imply shared live filesystem edits. Within each managed repository, `@` refers to that workspace's working-copy commit. Files and changes already present are your assigned starting state.\n\n");
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

    #[test]
    fn project_prompt_omits_ui_name() {
        let prompt =
            render_projects_prompt(&[(Utf8PathBuf::from("/repo/rho"), "Agent runtime".to_owned())]);
        assert!(prompt.contains("Path: /repo/rho"));
        assert!(prompt.contains("Description: Agent runtime"));
        assert!(!prompt.contains("name"));
    }

    fn workdir(path: &str, workspace_handle: Option<&str>) -> WorkdirPrompt {
        WorkdirPrompt {
            path: path.to_owned(),
            workspace_handle: workspace_handle.map(str::to_owned),
        }
    }

    #[test]
    fn managed_workspace_prompt_is_informational() {
        let prompt = render_workspace_prompt(&[workdir("/repo", Some("agentws"))]);
        assert!(prompt.contains("## Workspace Context"));
        assert!(prompt.contains("working directory is a Rho-managed jj workspace"));
        assert!(prompt.contains("`@` refers to that workspace's working-copy commit"));
        assert!(prompt.contains("assigned starting state"));
        assert!(!prompt.contains("Delegated Engineer Isolation"));
        assert!(!prompt.contains("do not create"));
    }

    #[test]
    fn live_workspace_prompt_reports_management() {
        let prompt = render_workspace_prompt(&[workdir("/repo", None)]);
        assert!(prompt.contains("live directories rather than Rho-managed jj workspaces"));
        assert!(prompt.contains("immediately visible to the user"));
    }

    #[test]
    fn workspace_prompt_lists_mixed_workdirs() {
        let prompt =
            render_workspace_prompt(&[workdir("/repo", Some("agentws")), workdir("/docs", None)]);
        assert!(prompt.contains("Workspace management differs across your working set"));
        assert!(prompt.contains("- /repo — Rho-managed jj workspace"));
        assert!(prompt.contains("- /docs — live directory"));
    }

    #[test]
    fn role_guidance_is_separate_from_the_base_prompt() {
        assert!(!BASE_PROMPT.contains("## PM"));
        assert!(!BASE_PROMPT.contains("## Advisor"));
        assert!(PM_BASE_PROMPT.contains("Do not inspect or modify"));
        assert!(PM_BASE_PROMPT.contains("Always delegate technical requests"));
        assert!(PM_BASE_PROMPT.contains("Use judgment"));
        assert!(PM_BASE_PROMPT.contains("user requests or suggests"));
        assert!(PM_BASE_PROMPT.contains("The intended asynchronous flow"));
        assert!(PM_BASE_PROMPT.contains("Do not poll or wait"));
        assert!(PM_BASE_PROMPT.contains("exactly, verbatim, and in full"));
        assert!(PM_BASE_PROMPT.contains("Additional context from PM:"));
        assert!(PM_BASE_PROMPT.contains("relay Engineer reports faithfully"));
        assert!(PM_BASE_PROMPT.contains("failed checks, or unresolved work"));
        assert!(PM_BASE_PROMPT.contains("Never claim work is complete"));
        assert!(ADVISOR_PROMPT.contains("advisory only"));
        assert!(PR_FRIENDLY_ENGINEER_PROMPT.starts_with("## Design Alignment"));
        assert!(
            PR_FRIENDLY_ENGINEER_PROMPT
                .contains("Before starting implementation, align with the user")
        );
        assert!(PR_FRIENDLY_ENGINEER_PROMPT.contains("normally one to three"));
        assert!(PR_FRIENDLY_ENGINEER_PROMPT.contains("Ask additional rounds"));
        assert!(
            PR_FRIENDLY_ENGINEER_PROMPT
                .contains("Do not ask about low-level implementation details")
        );
        assert!(PR_FRIENDLY_ENGINEER_PROMPT.contains("## Advisor Review"));
        assert!(
            PR_FRIENDLY_ENGINEER_PROMPT.contains("use `ask_advisor` for an independent review")
        );
        assert!(PR_FRIENDLY_ENGINEER_PROMPT.contains("before opening the pull request"));
        assert!(GITHUB_WORKFLOW_PROMPT.contains("`github-workflow` skill"));
        assert!(GITHUB_WORKFLOW_PROMPT.contains("Follow that skill through"));
        assert!(GITHUB_WORKFLOW_PROMPT.contains("Coordinating PMs"));
    }

    #[test]
    fn design_alignment_is_only_for_pr_friendly_engineers() {
        use crate::db::EngineerIntelligence;

        let engineer = claude_prompt(
            None,
            None,
            AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Medium,
                workflow: AgentWorkflow::PrFriendly,
            },
        );
        let pm = claude_prompt(
            None,
            None,
            AgentRole::WorkflowPM {
                workflow: AgentWorkflow::PrFriendly,
            },
        );
        let default_engineer = claude_prompt(
            None,
            None,
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium,
            },
        );

        assert!(engineer.contains("## Design Alignment"));
        assert!(engineer.contains("## GitHub Workflow"));
        assert!(!pm.contains("## Design Alignment"));
        assert!(pm.contains("## GitHub Workflow"));
        assert!(!default_engineer.contains("## Design Alignment"));
    }

    #[test]
    fn environment_prompt_mentions_working_directory() {
        let prompt = render_environment_prompt(&[workdir("/repo", Some("agentws"))]);
        assert!(prompt.contains("Working directory: /repo"));
        assert!(!prompt.contains("jj workspace id"));
        assert!(!prompt.contains("Additional workdirs"));
    }

    #[test]
    fn environment_prompt_lists_additional_workdirs() {
        let prompt = render_environment_prompt(&[
            workdir("/repo", Some("agentws")),
            workdir("/lib", Some("agentws")),
            workdir("/docs", None),
        ]);
        assert!(prompt.contains("Working directory: /repo"));
        assert!(prompt.contains("- /lib (your own checkout)"));
        assert!(prompt.contains("- /docs (the live directory"));
    }
}
