use std::sync::Arc;

use rho_agent_types::AgentRole;

use crate::log::AgentSpawnedBy;
use crate::multi_agent_tools::Team;

/// One model family's instructions, a whole document for each role. A
/// template's `{{team}}`, `{{user_owned_engineers}}` and `{{context}}` slots
/// take the agent's team, the section offered only to an agent the user
/// manages, and its workspace and project guidance.
struct Templates {
    engineer: &'static str,
    /// An Engineer working for another agent: it talks only to its parent,
    /// and neither delegates nor writes for the user.
    child: &'static str,
    advisor: &'static str,
    /// Offered only to an agent the user manages: one working for another
    /// agent must not open threads the user never asked for.
    user_owned_engineers: &'static str,
}

/// For the native runtime, which runs GPT models.
const GPT: Templates = Templates {
    engineer: include_str!("prompt/gpt/engineer.md"),
    child: include_str!("prompt/gpt/child.md"),
    advisor: include_str!("prompt/gpt/advisor.md"),
    user_owned_engineers: include_str!("prompt/gpt/user-owned-engineers.md"),
};

/// For the Claude Code runtime, which Rho's plugin delivers as the system
/// prompt (`rho_claude::plugin`).
const CLAUDE: Templates = Templates {
    engineer: include_str!("prompt/claude/engineer.md"),
    child: include_str!("prompt/claude/child.md"),
    advisor: include_str!("prompt/claude/advisor.md"),
    user_owned_engineers: include_str!("prompt/claude/user-owned-engineers.md"),
};

/// Fills a template's slots. The context goes in last, so project guidance
/// that happens to spell a slot stays as written.
fn render(template: &str, team: &str, user_owned: &str, context: &str) -> Arc<str> {
    template
        .replace("{{team}}", team)
        .replace("{{user_owned_engineers}}", user_owned)
        .replace("{{context}}", context)
        .into()
}

fn started_by_note(by: &str) -> String {
    format!(
        "Engineer {by} started you for the user; ask it with agents.message if you need more \
         context."
    )
}

/// Render an agent's role, project guidance, team, and environment for the
/// native runtime.
pub(crate) fn prompt(
    place: &WorksetPrompt,
    multi_agent: Option<&Team>,
    role: AgentRole,
) -> Arc<str> {
    templated(&GPT, Some(place), multi_agent, role)
}

/// The same for the Claude Code runtime.
pub(crate) fn claude_prompt(
    place: Option<&WorksetPrompt>,
    multi_agent: Option<&Team>,
    role: AgentRole,
) -> Arc<str> {
    templated(&CLAUDE, place, multi_agent, role)
}

fn templated(
    templates: &Templates,
    place: Option<&WorksetPrompt>,
    multi_agent: Option<&Team>,
    role: AgentRole,
) -> Arc<str> {
    let context = place.map_or_else(String::new, |place| {
        let (agents_files, skills) = &place.context;
        format!(
            "{}{}{}",
            render_workspace_prompt(place),
            render_agents_md_prompt(agents_files).unwrap_or_default(),
            render_skills_prompt(skills).unwrap_or_default(),
        )
    });
    let team = team_context(multi_agent, role);
    let child = multi_agent.is_some_and(|tools| !tools.spawned_by.user_owned());
    let user_owned = multi_agent
        .filter(|tools| tools.spawned_by.user_owned())
        .map_or("", |_| templates.user_owned_engineers);
    let template = match role {
        AgentRole::Engineer { .. } if child => templates.child,
        AgentRole::Engineer { .. } => templates.engineer,
        AgentRole::Advisor { .. } => templates.advisor,
    };
    render(template, &team, user_owned, &context)
}

fn team_context(multi_agent: Option<&Team>, role: AgentRole) -> String {
    multi_agent.map_or_else(String::new, |tools| {
        let agent_id = &tools.agent;
        let identity = match tools.parent.as_ref() {
            Some(parent) => format!(
                "You are an agent in a team of agents collaborating to complete a task. Your \
                 agent id is {agent_id}; your parent agent is {}.\n\nMessages from your \
                 parent define your task. Report to your parent with agents.message.",
                parent
            ),
            None => format!(
                "You are the primary agent in a team of agents collaborating to fulfill the \
                 user's goals. Your agent id is {agent_id}."
            ),
        };
        if matches!(role, AgentRole::Advisor { .. }) {
            return format!(
                "{identity}

Complete your independent analysis and send it to your parent with agents.message. \
Ask a known agent for context the same way, and end your turn when blocked on a reply.
"
            );
        }
        let ownership = match (tools.spawned_by, tools.started_by.as_deref()) {
            (AgentSpawnedBy::Engineer, _) => {
                "You were spawned by another Engineer. Own the bounded assignment in the \
                 parent message."
                    .to_owned()
            }
            // An Engineer started for the user is the user's like any other;
            // it only learns who holds the context behind its brief.
            (AgentSpawnedBy::UserOwned { .. }, Some(by)) => format!(
                "{} You own the user's technical outcome.",
                started_by_note(by)
            ),
            (AgentSpawnedBy::Direct | AgentSpawnedBy::UserOwned { .. }, _) => {
                "You were started directly and own the user's technical outcome.".to_owned()
            }
        };
        let message_tool = "agents.message";
        format!(
            "{identity}

{ownership}

You will receive agent messages in this format:
```
Message Type: MESSAGE
Sender: <agent id>
Payload:
<payload text>
```

Use `{message_tool}` for bidirectional communication with any known agent.

"
        )
    })
}

/// An agent's place as the prompt renders it.
pub(crate) struct WorksetPrompt {
    /// The workset directory as the agent sees it.
    root: String,
    /// The agent's working directory as it sees it.
    cwd: String,
    /// Whether the working directory is inside a git checkout.
    git: bool,
    context: (
        Vec<rho_context_config::AgentsFile>,
        Vec<rho_context_config::Skill>,
    ),
}

impl WorksetPrompt {
    /// Worker paths are already in the workset namespace. A checkout's
    /// root counts where the working directory is: inside the workset only
    /// within it, and a host directory's wherever its checkout begins.
    pub fn new(cwd: &camino::Utf8Path) -> Self {
        let roots = rho_fs_view::resolve_workdir_root(cwd.as_std_path()).map(|(root, _)| {
            let in_workset = cwd.starts_with(rho_fs_view::MOUNT_ROOT);
            let root = if !in_workset || root.starts_with(rho_fs_view::MOUNT_ROOT) {
                root
            } else {
                cwd.to_owned()
            };
            (root.clone(), root)
        });
        Self::discover(cwd, roots)
    }

    /// A preview outside the worker: `cwd` as the agent sees it, with its
    /// files read from `backing`, the same directory as this process
    /// reaches it. The checkout root is as many levels above `cwd` as it is
    /// above `backing`.
    pub fn backed(cwd: &camino::Utf8Path, backing: &camino::Utf8Path) -> Self {
        let roots = (|| {
            let (root, _) = rho_fs_view::resolve_workdir_root(backing.as_std_path())?;
            let backing = camino::Utf8PathBuf::try_from(backing.as_std_path().canonicalize()?)?;
            let depth = backing.strip_prefix(&root)?.components().count();
            let visible = cwd
                .ancestors()
                .nth(depth)
                .ok_or_else(|| anyhow::anyhow!("{root} is above the agent's view of {cwd}"))?;
            anyhow::Ok((visible.to_owned(), root))
        })();
        Self::discover(cwd, roots)
    }

    fn discover(
        cwd: &camino::Utf8Path,
        roots: anyhow::Result<(camino::Utf8PathBuf, camino::Utf8PathBuf)>,
    ) -> Self {
        let mut place = Self {
            root: rho_fs_view::MOUNT_ROOT.into(),
            cwd: cwd.to_string(),
            git: false,
            context: (Vec::new(), Vec::new()),
        };
        let (visible_root, root) = match roots {
            Ok(roots) => roots,
            Err(error) => {
                eprintln!("rho-agent: context discovery: {error:#}");
                return place;
            }
        };
        place.git = root.join(".git").exists();
        let context = rho_context_config::DiscoveredContext::discover(&visible_root, &root);
        for diagnostic in &context.diagnostics {
            eprintln!(
                "rho-agent: context config {:?}: {}: {}",
                diagnostic.kind,
                diagnostic.path.display(),
                diagnostic.message
            );
        }
        place.context = (context.agents_files, context.skills);
        place
    }
}

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

    let mut roots = Vec::new();
    let mut entries = String::new();
    for skill in skills {
        let path = if let Some(root) = skill.file_path.parent().and_then(|dir| dir.parent()) {
            let index = roots
                .iter()
                .position(|existing| *existing == root)
                .unwrap_or_else(|| {
                    roots.push(root);
                    roots.len() - 1
                });
            format!(
                "r{index}/{}",
                skill.file_path.strip_prefix(root).expect("ancestor root")
            )
        } else {
            skill.file_path.to_string()
        };
        entries.push_str(&format!(
            "- {}: {} (file: {path})\n",
            skill.name, skill.description
        ));
    }
    let mut out = String::from(
        "## Skills\n\nA skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and a path. Short paths can be expanded into absolute paths using the skill roots table.\n\nTo use a skill, call `skills.load(path)` with its directory or its `SKILL.md`, not a plain file read. It shows you the skill, and shows it to you again after each compaction.\n\n",
    );
    if !roots.is_empty() {
        out.push_str("### Skill roots\n\n");
        for (index, root) in roots.iter().enumerate() {
            out.push_str(&format!("- `r{index}` = `{root}`\n"));
        }
        out.push('\n');
    }
    out.push_str("### Available skills\n\n");
    out.push_str(&entries);
    out.push('\n');
    Some(out)
}

fn render_workspace_prompt(place: &WorksetPrompt) -> String {
    let WorksetPrompt { root, cwd, .. } = place;
    let mut out = format!(
        "## Workspace Context

Working directory: {cwd}

Your workset is the directory {root}, holding the repositories you work in. Relative paths in \
commands and patches resolve against your working directory. Stay within the workset unless the \
user points you elsewhere.

Clone further repositories into the workset with \
`git clone <url>` (fast: clones are born from a local mirror, and `git fetch` reads it), and \
add checkouts of a repository with `git worktree add`. Nothing in the workset is cleaned up \
behind you; what is there when you start is the starting state you were given.

"
    );
    if place.git {
        out.push_str("This repository is a git checkout; `origin` is the real remote.\n\n");
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
    fn renders_skill_catalogue_with_stable_root_aliases() {
        let mut external = skill("beta", "Other skill");
        external.file_path = "/opt/shared/skills/beta/SKILL.md".into();
        let prompt = render_skills_prompt(&[
            skill("zeta", "Last skill"),
            external,
            skill("alpha", "First skill"),
        ])
        .unwrap();
        assert_eq!(
            prompt,
            concat!(
                "## Skills\n\n",
                "A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and a path. Short paths can be expanded into absolute paths using the skill roots table.\n\n",
                "To use a skill, call `skills.load(path)` with its directory or its `SKILL.md`, not a plain file read. It shows you the skill, and shows it to you again after each compaction.\n\n",
                "### Skill roots\n\n",
                "- `r0` = `/repo/.agents/skills`\n",
                "- `r1` = `/opt/shared/skills`\n\n",
                "### Available skills\n\n",
                "- alpha: First skill (file: r0/alpha/SKILL.md)\n",
                "- beta: Other skill (file: r1/beta/SKILL.md)\n",
                "- zeta: Last skill (file: r0/zeta/SKILL.md)\n\n",
            )
        );
        assert!(render_skills_prompt(&[]).is_none());
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

    fn place(git: bool) -> WorksetPrompt {
        WorksetPrompt {
            root: "/src".to_owned(),
            cwd: "/src/repo".to_owned(),
            git,
            context: (Vec::new(), Vec::new()),
        }
    }

    #[test]
    fn workspace_prompt_is_informational() {
        let prompt = render_workspace_prompt(&place(true));
        assert!(prompt.contains("## Workspace Context"));
        assert!(prompt.contains("Your workset is the directory /src"));
        assert!(prompt.contains("Working directory: /src/repo"));
        assert!(prompt.contains("Relative paths in commands and patches"));
        assert!(prompt.contains("git clone <url>"));
        assert!(prompt.contains("git worktree add"));
        assert!(prompt.contains("This repository is a git checkout"));
        assert!(prompt.contains("starting state you were given"));
        assert!(!prompt.contains("Other checkouts"));
        assert!(!prompt.contains("share"));
        assert!(!prompt.contains("disposable environment"));
        assert!(!prompt.contains("agent that started you"));
        assert!(!prompt.contains("do not create"));
    }

    #[test]
    fn workspace_prompt_omits_git_section_when_absent() {
        let prompt = render_workspace_prompt(&place(false));
        assert!(!prompt.contains("This repository is a git checkout"));
        assert!(!prompt.contains("disposable environment"));
        assert!(!prompt.contains("agent that started you"));
    }

    #[test]
    fn engineer_prompt_integrates_capabilities_in_story_order() {
        let prompt = render(GPT.engineer, "TEAM_SENTINEL\n\n", "", "WORKSPACE_SENTINEL");
        let headings = prompt
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect::<Vec<_>>();
        assert_eq!(
            headings,
            [
                "## Autonomy And Persistence",
                "## Engineering Principles",
                "## Discovery Discipline",
                "## Verification",
                "## Actions Requiring Explicit Approval",
                "## Acting and talking",
                "## How time works",
                "## The notebook",
                "## Working with other agents",
                "## Working with the user",
                "## Diagrams",
            ]
        );
        let discovery = prompt
            .split("## Discovery Discipline")
            .nth(1)
            .unwrap()
            .split("## Verification")
            .next()
            .unwrap();
        assert!(discovery.contains("web.run"));
        assert!(
            discovery
                .find("Follow relevant project guidance and skills")
                .unwrap()
                < discovery.find("### External research").unwrap()
        );
        assert!(discovery.contains("clone the upstream repository into your workset"));

        let verification = prompt
            .split("## Verification")
            .nth(1)
            .unwrap()
            .split("## Actions Requiring")
            .next()
            .unwrap();
        assert!(verification.contains("def view_image("));
        let collaboration = prompt
            .split("## Working with other agents")
            .nth(1)
            .unwrap()
            .split("## Working with the user")
            .next()
            .unwrap();
        assert!(collaboration.contains("### Advisor"));
        assert!(
            collaboration.find("### Advisor").unwrap()
                < collaboration.find("### Engineers").unwrap()
        );
        assert!(
            collaboration.find("agents.message(*").unwrap()
                < collaboration
                    .find("### Briefing and integrating work")
                    .unwrap()
        );
        assert!(collaboration.contains("agents.spawn_new_advisor(msg: str)"));
        assert!(collaboration.starts_with("\n\nTEAM_SENTINEL\n\n"));
        assert!(collaboration.contains("### Engineers"));
        assert!(!collaboration.contains("share a checkout"));
        assert!(collaboration.contains("agents.spawn_new_engineer(*, task_name:"));
        assert!(collaboration.contains("task_name is a short kebab-case label"));
        assert!(
            collaboration.contains("loads applicable AGENTS.md guidance and the skill catalogue")
        );
        assert!(collaboration.contains("agents.cancel("));
        assert!(collaboration.contains("agents.message("));
        assert!(!prompt.contains("transcript"));
        assert!(!prompt.contains("commentary"));
        assert!(prompt.contains("Every response is exactly one exec call"));
        assert!(prompt.contains("human.send(text, *, kind)"));
        assert!(prompt.contains("end_turn()"));
        assert!(!prompt.contains(".reply()"));
        assert!(prompt.contains("Task.from_session_id(session_id: int) → Task"));
        assert!(!prompt.contains("suppress_tool_wakeups"));
        assert!(prompt.contains("await handle → CommandExit(id: int, exit_code: int | None)"));
        assert!(prompt.contains("returns a persistent command handle"));
        let execution = prompt
            .split("## Acting and talking")
            .nth(1)
            .unwrap()
            .split("## Working with other agents")
            .next()
            .unwrap();
        assert!(execution.contains("automatically.\ncommand(cmd:"));
        assert!(!execution.contains('`'));
        assert!(!execution.contains("CommandResult"));

        assert!(
            collaboration.contains("Without an explicit request, do NOT consult the Advisor for:")
        );
        assert!(collaboration.contains("Choose between plausible type-boundary designs"));
        assert!(prompt.contains("def papercut("));
        assert!(prompt.ends_with("WORKSPACE_SENTINEL"));
        for legacy in [
            "Python Code Mode",
            "Available tools:",
            "Arguments: ",
            "### Rho agents",
            "class agents:",
            "from collections.abc import",
            "from typing import",
        ] {
            assert!(!prompt.contains(legacy), "{legacy}");
        }
    }

    #[test]
    fn advisor_prompt_has_its_own_policy_and_only_its_capabilities() {
        let prompt = render(GPT.advisor, "TEAM_SENTINEL\n\n", "", "");
        for section in [
            "## Read before you advise",
            "## Work quickly",
            "## Review stance",
            "## Engineering judgment",
            "## Debugging",
            "## Advisory mode",
            "## Discovery discipline",
            "## Acting and talking",
            "## How time works",
            "## The notebook",
            "## Working with other agents",
            "## Shape the response",
            "## Communication",
        ] {
            assert!(prompt.contains(section), "missing {section}");
        }
        assert!(prompt.contains("Do not rerun tests, builds, or checks"));
        assert!(prompt.contains("narrowly scoped scratch edit"));
        assert!(prompt.contains("No blockers"));
        assert!(prompt.contains("## Working with other agents\n\nTEAM_SENTINEL\n\n"));
        assert!(prompt.contains("agents.message("));
        assert!(prompt.contains("Use `web.run` for web searches and reading web pages."));
        assert!(prompt.contains("focused result with agents.message"));
        assert!(!prompt.contains("transcript"));
        for forbidden in [
            "spawn_new_advisor",
            "spawn_new_engineer",
            "agents.cancel(*",
            "Autonomy And Persistence",
            "human.",
            "archive()",
            "## Diagrams",
            "TypeScript",
            "Python Code Mode",
            "Available tools:",
        ] {
            assert!(!prompt.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn engineer_delegation_requires_concrete_benefit_in_both_runtimes() {
        for prompt in [
            render(GPT.engineer, "", "", ""),
            claude_prompt(None, None, AgentRole::default()),
        ] {
            assert!(prompt.contains(" Engineers\n\n```python\nagents.spawn_new_engineer("));
            assert!(
                prompt.find("agents.spawn_new_engineer(").unwrap()
                    < prompt.find("When to use an Engineer:").unwrap()
            );
            for rule in [
                "loads applicable AGENTS.md guidance and the skill catalogue",
                "agents.spawn_new_engineer(*, task_name: str, prompt: str, workdir: str)",
                "concrete benefit beyond",
                "keep the critical path local",
                "concrete sidecar task",
                "that is not independent work",
                "leave you only coordinating while others implement the main change",
                "one bounded unit is massive enough",
                "merely working on agent-related features does not count",
                "complex, multi-step, cross-package, or touches many files",
                "serial handoff with no meaningful parallelism or context-isolation benefit",
                "routine review or verification of your own work",
                "reading a single file, performing an exact text search",
                "bounded research assignments are still appropriate",
                "not the whole user request merely because you already wrote",
                "suitable existing Engineer",
                "single-threaded unless write targets are clearly disjoint or isolated",
                "whatever you find.",
                "exact quotes, numbers, URLs, or file paths",
                "relevant combined validation",
            ] {
                assert!(prompt.contains(rule), "missing {rule}");
            }
            for inappropriate in [
                "Prefer parallel Tasks for verification",
                "Never delegate understanding",
                "can't communicate with it until it finishes",
                "worker's intermediate work is discarded",
            ] {
                assert!(!prompt.contains(inappropriate), "{inappropriate}");
            }
        }
        for prompt in [
            render(GPT.advisor, "", "", ""),
            claude_prompt(
                None,
                None,
                AgentRole::Advisor {
                    intelligence: rho_agent_types::AdvisorIntelligence::Medium,
                },
            ),
        ] {
            assert!(!prompt.contains("When to use an Engineer:"));
        }
    }

    #[test]
    fn agent_messaging_scopes_replies_and_avoids_duplicate_completion_reports() {
        for prompt in [
            render(GPT.engineer, "", "", ""),
            claude_prompt(None, None, AgentRole::default()),
        ] {
            let communication = prompt.split("# Working with other agents").nth(1).unwrap();
            for rule in [
                "send findings, questions, or a scoped next action",
                "say whether another reply is needed",
                "Stop exchanging messages when\nthe requested work is complete",
                "do not create acknowledgment loops",
            ] {
                assert!(communication.contains(rule), "missing {rule}");
            }
        }
        let parent = render(GPT.engineer, "", "", "");
        assert!(parent.contains("it reports back to you with agents.message"));
        assert!(!parent.contains("mailed to your parent"));
    }

    #[test]
    fn execution_examples_cover_commands_and_live_cells_for_each_role() {
        let native = [
            render(GPT.engineer, "", "", ""),
            render(GPT.advisor, "", "", ""),
        ];
        let claude = [
            claude_prompt(None, None, AgentRole::default()),
            claude_prompt(
                None,
                None,
                AgentRole::Advisor {
                    intelligence: rho_agent_types::AdvisorIntelligence::Medium,
                },
            ),
        ];
        for prompt in native.iter().chain(&claude) {
            assert!(!prompt.contains("set_checkin"));
            assert!(!prompt.contains("suppress_tool_wakeups"));
            for example in [
                "set_max_wait(seconds: int) → None",
                "web.run(**request) → Awaitable[str]",
                "preloaded in Python",
                r#"web.run(search_query=[{"q": "search terms"}])"#,
                r#"web.run(open=[{"ref_id": "https://example.com"}])"#,
                "    command(\"git diff --stat\")\n    command(\"rg -n 'TODO' src\")",
                "    check = await command(\"cargo check\")\n    if check.exit_code == 0:",
            ] {
                assert!(prompt.contains(example), "{example}");
            }
        }
        for prompt in &native {
            for rule in [
                "Your latest exec finishing wakes you immediately",
                "After end_turn(), only messages, notify(), and task failures wake you",
                "asyncio.create_task(coro)",
            ] {
                assert!(prompt.contains(rule), "{rule}");
            }
        }
        assert!(claude[0].contains("human.send(text, *, kind)"));
        for prompt in &claude {
            assert!(prompt.contains("only the first exec call in a\nresponse runs"));
            assert!(!prompt.contains("mcp__py__exec"));
            assert!(prompt.contains("The check-in comes 120 seconds after your last response"));
        }
    }

    #[test]
    fn claude_uses_shared_policy_with_mcp_transport_for_both_roles() {
        for role in [
            AgentRole::default(),
            AgentRole::Advisor {
                intelligence: rho_agent_types::AdvisorIntelligence::Medium,
            },
        ] {
            let team = Team {
                agent: "eng-child".into(),
                parent: Some("eng-parent".into()),
                spawned_by: AgentSpawnedBy::Engineer,
                started_by: None,
            };
            let prompt = claude_prompt(None, Some(&team), role);
            assert!(prompt.contains("eng-child"));
            assert!(prompt.contains("eng-parent"));
            assert!(prompt.contains("only the first exec call in a\nresponse runs"));
            assert!(!prompt.contains("mcp__py__exec"));
            assert!(prompt.contains("agents.message(*, agent_id: str"));
            assert!(prompt.contains("end_turn()"));
            assert!(prompt.contains("the final text of a Claude Code turn is\nnot delivered"));
            // A child talks only to its parent and never delegates.
            for forbidden in [
                "human.",
                "archive()",
                "agents.spawn_new_engineer(*",
                "## Diagrams",
            ] {
                assert!(!prompt.contains(forbidden), "{forbidden}");
            }
        }
    }

    #[test]
    fn a_child_engineer_keeps_the_engineering_policy_and_reports_to_its_parent() {
        let prompt = render(GPT.child, "TEAM_SENTINEL\n\n", "", "WORKSPACE_SENTINEL");
        let headings = prompt
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect::<Vec<_>>();
        assert_eq!(
            headings,
            [
                "## Owning the assignment",
                "## Engineering Principles",
                "## Discovery Discipline",
                "## Verification",
                "## Actions Requiring Approval",
                "## Acting and talking",
                "## How time works",
                "## The notebook",
                "## Working with other agents",
                "## Reporting to your parent",
            ]
        );
        assert!(prompt.contains("## Working with other agents\n\nTEAM_SENTINEL\n\n"));
        assert!(prompt.contains("def papercut("));
        assert!(prompt.ends_with("WORKSPACE_SENTINEL"));
        for forbidden in [
            "human.",
            "archive()",
            "spawn_new_advisor",
            "spawn_new_engineer",
        ] {
            assert!(!prompt.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn only_agents_the_user_manages_may_start_engineers_for_the_user() {
        let team = |spawned_by, parent: Option<&str>, started_by: Option<&str>| Team {
            agent: "eng-self".into(),
            parent: parent.map(Into::into),
            spawned_by,
            started_by: started_by.map(Into::into),
        };
        let by =
            rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let direct = team(AgentSpawnedBy::Direct, None, None);
        let user_owned = team(AgentSpawnedBy::UserOwned { by }, None, Some("eng-starter"));
        let child = team(AgentSpawnedBy::Engineer, Some("eng-parent"), None);
        let signature = "agents.spawn_user_owned_engineer(*, task_name:";
        for team in [&direct, &user_owned] {
            let prompt = claude_prompt(None, Some(team), AgentRole::default());
            assert!(prompt.contains(signature));
            assert!(prompt.find("# Engineers\n").unwrap() < prompt.find(signature).unwrap());
            // Whoever spawned it, an Engineer the user manages talks to the
            // user.
            assert!(
                prompt.contains("human.send(text, *, kind)") && prompt.contains("# Communication")
            );
        }
        for prompt in [
            claude_prompt(None, Some(&child), AgentRole::default()),
            claude_prompt(
                None,
                Some(&direct),
                AgentRole::Advisor {
                    intelligence: rho_agent_types::AdvisorIntelligence::Medium,
                },
            ),
        ] {
            assert!(!prompt.contains(signature));
        }
        // One started for the user is told nothing a direct one is not,
        // but who can give it the context behind its brief.
        let note = " Engineer eng-starter started you for the user; ask it with agents.message \
                    if you need more context.";
        assert!(claude_prompt(None, Some(&user_owned), AgentRole::default()).contains(note.trim()));

        // The native prompt offers it after the Engineers it manages and
        // before briefing, which applies to both.
        let native = render(GPT.engineer, "", GPT.user_owned_engineers, "");
        let engineers = native.find("### Engineers\n").unwrap();
        let owned = native.find("### Engineers the user manages").unwrap();
        let briefing = native.find("### Briefing and integrating work").unwrap();
        assert!(engineers < owned && owned < briefing);
    }

    #[test]
    fn every_template_fills_its_slots() {
        for templates in [&GPT, &CLAUDE] {
            for template in [templates.engineer, templates.child, templates.advisor] {
                let prompt = render(template, "TEAM_SENTINEL", "", "CONTEXT_SENTINEL");
                assert!(prompt.contains("TEAM_SENTINEL"));
                assert!(prompt.ends_with("CONTEXT_SENTINEL"));
                assert!(!prompt.contains("{{"), "an unfilled slot");
            }
            let engineer = render(templates.engineer, "", "OWNED_SENTINEL", "");
            assert!(engineer.contains("OWNED_SENTINEL"));
        }
    }
}
