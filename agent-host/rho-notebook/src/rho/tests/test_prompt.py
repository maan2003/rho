"""The behavior and assembly of the default model instructions."""

import os
import tempfile
import unittest
from unittest import mock

from . import fake_rho
from .support import add_agent, claude, native

import rho
from rho import prompt, skills, workset
from rho.prompt import render, template

GPT, CLAUDE = "gpt", "claude"

def gpt(name, team="", owned="", context=""):
    return render(template(GPT, name), team, owned, context)


def claude_t(name, team="", owned="", context=""):
    return render(template(CLAUDE, name), team, owned, context)


def between(text, start, end):
    return text.split(start, 1)[1].split(end, 1)[0]


class Templates(unittest.TestCase):
    def test_engineer_chat_contract(self):
        for family in [GPT, CLAUDE]:
            text = render(template(family, "engineer"), "", "", "")
            acting = between(text, "Acting and talking", "Before ending a turn")
            for rule in [
                "chat.send(text, kind, *, thread) → int",
                "chat.start(project, text, kind) → int",
                "chat.fork(message, text, kind) → int",
                "does the user need to read this?",
                "It makes a card on their desk and stays in the record.",
                "It never makes a card.",
                "once something newer exists in the thread",
                "edit it with new findings",
                "read the thread with `chat.read`",
                "Say what you answer in the first line",
                "under `## proof`",
                "last output lines verbatim in a code block",
                "a screenshot if the change is visible",
                "Another agent is reached with `agents.message`, never through chat.",
            ]:
                self.assertIn(rule, acting, family)
            for obsolete in ["mail.", "human.", "message_agent", "chat.direct", "ping=", "replace_last", "reply_to", "## recap"]:
                self.assertNotIn(obsolete, text, family)
            communication = text.split("# Communication", 1)[1] if family == CLAUDE else text.split("## Working with the user", 1)[1]
            self.assertIn("A result message must be fully self-contained", communication, family)
            self.assertIn('for kind="message"; flashes are plain prose and the host does not check them', communication, family)

    def test_every_template_fills_its_slots(self):
        for family in [GPT, CLAUDE]:
            for name in ["engineer", "child", "advisor"]:
                text = render(template(family, name), "TEAM_SENTINEL", "", "CONTEXT_SENTINEL")
                self.assertIn("TEAM_SENTINEL", text)
                self.assertTrue(text.endswith("CONTEXT_SENTINEL"))
                self.assertNotIn("{{", text)
            self.assertIn("OWNED_SENTINEL", render(template(family, "engineer"), "", "OWNED_SENTINEL", ""))

    def test_context_that_spells_a_slot_stays_as_written(self):
        text = gpt("engineer", "T", "", "guide says {{team}}")
        self.assertTrue(text.endswith("guide says {{team}}"))

    def test_engineer_prompt_integrates_capabilities_in_story_order(self):
        text = gpt("engineer", "TEAM_SENTINEL\n\n", "", "WORKSPACE_SENTINEL")
        headings = [line for line in text.splitlines() if line.startswith("## ")]
        self.assertEqual(headings, [
            "## Autonomy And Persistence", "## Engineering Principles", "## Discovery Discipline",
            "## Verification", "## Actions Requiring Explicit Approval", "## Acting and talking",
            "## How time works", "## The notebook", "## Working with other agents",
            "## Working with the user", "## Diagrams",
        ])
        discovery = between(text, "## Discovery Discipline", "## Verification")
        self.assertIn("web.run", discovery)
        self.assertLess(discovery.index("Follow relevant project guidance and skills"), discovery.index("### External research"))
        self.assertIn("def view_image(", between(text, "## Verification", "## Actions Requiring"))
        collaboration = between(text, "## Working with other agents", "## Working with the user")
        self.assertTrue(collaboration.startswith("\n\nTEAM_SENTINEL\n\n"))
        self.assertLess(collaboration.index("### Advisor"), collaboration.index("### Engineers"))
        for needle in ["agents.spawn_new_advisor(msg: str)", "agents.spawn_new_engineer(*, task_name:",
                       "task_name is a short kebab-case label", "agents.cancel(",
                       "loads applicable AGENTS.md guidance and the skill catalogue",
                       "Without an explicit request, do NOT consult the Advisor for:"]:
            self.assertIn(needle, collaboration)
        self.assertIn('chat.send(text, kind, *, thread) → int', text)
        self.assertNotIn('mail.status', text)
        self.assertNotIn("Post passing progress", text)
        self.assertIn("A kept message cannot contain a sentence of more than 25 words", text)
        self.assertIn("it reports back to you with `agents.message`.", text)
        self.assertIn("Task.from_session_id(session_id: int) → Task", text)
        self.assertIn("def papercut(", text)
        self.assertTrue(text.endswith("WORKSPACE_SENTINEL"))
        for gone in ["human.", "chat.direct", "transcript", "Python Code Mode", "class agents:"]:
            self.assertNotIn(gone, text)

    def test_advisor_prompt_has_only_its_capabilities(self):
        text = gpt("advisor", "TEAM_SENTINEL\n\n")
        self.assertIn("## Working with other agents\n\nTEAM_SENTINEL\n\n", text)
        self.assertIn("focused result with mail.send", text)
        self.assertIn("mail.send(agent, text, *, reply_to=None) → int", text)
        self.assertIn("Use `mail.send` when a known agent has context", text)
        for forbidden in ["spawn_new_advisor", "spawn_new_engineer", "agents.cancel(*", "human.",
                          "archive()", "## Diagrams", "chat.direct"]:
            self.assertNotIn(forbidden, text)

    def test_a_child_keeps_the_engineering_policy_and_reports_to_its_parent(self):
        text = gpt("child", "TEAM_SENTINEL\n\n", "", "WORKSPACE_SENTINEL")
        headings = [line for line in text.splitlines() if line.startswith("## ")]
        self.assertEqual(headings, [
            "## Owning the assignment", "## Engineering Principles", "## Discovery Discipline",
            "## Verification", "## Actions Requiring Approval", "## Acting and talking",
            "## How time works", "## The notebook", "## Working with other agents",
            "## Reporting to your parent",
        ])
        self.assertIn("Your parent agent sees only what you\nsend it with mail.send, not your code", text)
        for forbidden in ["human.", "archive()", "spawn_new_advisor", "spawn_new_engineer", "chat.direct"]:
            self.assertNotIn(forbidden, text)

    def test_engineer_delegation_rules_in_both_families(self):
        for text in [gpt("engineer"), claude_t("engineer")]:
            self.assertIn(" Engineers\n\n```python\nagents.spawn_new_engineer(", text)
            self.assertLess(text.index("agents.spawn_new_engineer("), text.index("When to use an Engineer:"))
            communication = text.split("# Working with other agents", 1)[1]
            for rule in ["say whether another reply is needed",
                         "Stop exchanging messages when\nthe requested work is complete",
                         "do not create acknowledgment loops"]:
                self.assertIn(rule, communication)
        for text in [gpt("advisor"), claude_t("advisor")]:
            self.assertNotIn("When to use an Engineer:", text)

    def test_claude_transport_wording(self):
        for name in ["engineer", "child", "advisor"]:
            text = claude_t(name)
            self.assertIn("only the first exec call in a\nresponse runs", text)
            self.assertNotIn("mcp__py__exec", text)
            self.assertIn("The check-in comes 120 seconds after your last response", text)


class Sections(unittest.TestCase):
    def test_skill_catalogue_with_stable_root_aliases(self):
        found = [
            {"name": "zeta", "description": "Last skill", "path": "/repo/.agents/skills/zeta/SKILL.md"},
            {"name": "beta", "description": "Other skill", "path": "/opt/shared/skills/beta/SKILL.md"},
            {"name": "alpha", "description": "First skill", "path": "/repo/.agents/skills/alpha/SKILL.md"},
        ]
        self.assertEqual(prompt.skills(found), (
            "## Skills\n\n"
            "A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and a path. Short paths can be expanded into absolute paths using the skill roots table.\n\n"
            "To use a skill, call `skills.load(path)` with its directory or its `SKILL.md`, not a plain file read. It shows you the skill. A skill can ask to be shown again after each compaction.\n\n"
            "### Skill roots\n\n"
            "- `r0` = `/repo/.agents/skills`\n"
            "- `r1` = `/opt/shared/skills`\n\n"
            "### Available skills\n\n"
            "- alpha: First skill (file: r0/alpha/SKILL.md)\n"
            "- beta: Other skill (file: r1/beta/SKILL.md)\n"
            "- zeta: Last skill (file: r0/zeta/SKILL.md)\n\n"
        ))
        self.assertEqual(prompt.skills([]), "")

    def test_agents_md_with_file_boundaries(self):
        text = prompt.agents_md([{"path": "/repo/AGENTS.md", "content": "Read the docs."},
                                 {"path": "/repo/a/AGENTS.md", "content": "Inner.\n"}])
        self.assertTrue(text.startswith("## AGENTS.md instructions\nThe following instructions were loaded"))
        self.assertIn('<AGENTS_FILE path="/repo/AGENTS.md">\nRead the docs.\n</AGENTS_FILE>\n\n<AGENTS_FILE path="/repo/a/AGENTS.md">\nInner.\n</AGENTS_FILE>\n\n', text)
        self.assertIn("follow them unless they conflict", text)
        self.assertEqual(prompt.agents_md([]), "")

    def test_workspace_section(self):
        text = prompt.workspace("/src", "/src/repo", True)
        self.assertTrue(text.startswith("## Workspace Context\n\nWorking directory: /src/repo\n\nYour workset is the directory /src, holding"))
        self.assertIn("Relative paths in commands and patches", text)
        self.assertIn("`git clone <url>` (fast: clones", text)
        self.assertTrue(text.endswith("starting state you were given.\n\nThis repository is a git checkout; `origin` is the real remote.\n\n"))
        self.assertNotIn("git checkout", prompt.workspace("/src", "/src/repo", False))

    def test_a_checkout_counts_inside_the_workset_only_when_it_begins_there(self):
        with tempfile.TemporaryDirectory() as top:
            outer = os.path.join(top, "outer")
            root = os.path.join(outer, "ws")
            inner = os.path.join(root, "repo", "sub")
            os.makedirs(inner)
            os.makedirs(os.path.join(outer, ".git"))
            plain = os.path.join(root, "plain")
            os.makedirs(plain)
            # Only the checkout above the workset: does not count.
            self.assertFalse(prompt.in_checkout(root, inner))
            os.makedirs(os.path.join(root, "repo", ".git"))
            self.assertTrue(prompt.in_checkout(root, inner))
            self.assertFalse(prompt.in_checkout(root, plain))
            # Outside the workset, a checkout counts wherever it begins.
            self.assertTrue(prompt.in_checkout(os.path.join(top, "elsewhere"), plain))


class Instructions(unittest.TestCase):
    def setUp(self):
        fake_rho.reset()
        self.state = fake_rho.STATE
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        patcher = mock.patch.object(workset, "root", self.dir.name)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.cwd = os.path.join(self.dir.name, "repo")
        os.makedirs(os.path.join(self.cwd, ".git"))
        self.state.contexts[self.cwd] = {
            "agents_files": [{"path": self.cwd + "/AGENTS.md", "content": "Use tabs."}],
            "skills": [{"name": "s", "description": "d", "path": self.cwd + "/.agents/skills/s/SKILL.md"}],
        }

    def agent(self, runtime, **props):
        return rho.agent(add_agent(self.state, runtime, cwd=self.cwd, **props))

    def test_a_direct_native_engineer_gets_the_gpt_engineer_prompt_whole(self):
        agent = self.agent(native(), preset="engineer")
        team = (
            f"You are the primary agent in a team of agents collaborating to fulfill the user\'s goals. Your agent id is eng-{agent.id}."
            "\n\nYou were started directly and own the user\'s technical outcome."
            "\n\nAnother agent's message to you arrives in this format:\n```\n[message from <agent id>]\n<text>\n```"
            '\n\nReply with `agents.message(agent_id=sender, message=text)`, where `sender` is the agent ID from the header.\n\n'
        )
        context = (
            prompt.workspace(self.dir.name, self.cwd, True)
            + f'## AGENTS.md instructions\nThe following instructions were loaded from AGENTS.md files. They are user/project instructions: follow them unless they conflict with higher-priority system or developer instructions. More specific files appear later and usually override broader ones.\n\n<AGENTS_FILE path="{self.cwd}/AGENTS.md">\nUse tabs.\n</AGENTS_FILE>\n\n'
            + prompt.skills(self.state.contexts[self.cwd]["skills"])
        )
        expected = gpt("engineer", team, template(GPT, "user-owned-engineers"), context)
        self.assertEqual(prompt.instructions(agent), expected)
        self.assertIn("This repository is a git checkout", expected)
        self.assertIn("### Engineers the user manages", expected)

    def test_the_family_follows_the_runtime(self):
        native_text = prompt.instructions(self.agent(native(), preset="engineer"))
        claude_text = prompt.instructions(self.agent(claude(), preset="engineer"))
        self.assertTrue(native_text.startswith("You are Rho, an autonomous coding agent. You and the user share one workspace."))
        self.assertTrue(claude_text.startswith("You are Rho, an autonomous coding agent. You pair program with the user"))

    def test_a_child_reports_to_its_parent(self):
        parent = self.agent(native(), preset="engineer")
        for runtime in [native(), claude()]:
            child = self.agent(runtime, preset="child", parent=parent.id)
            text = prompt.instructions(child)
            self.assertIn(f"Your agent id is eng-{child.id}; your parent agent is eng-{parent.id}.\n\nMessages from your parent define your task. Report to your parent with mail.send('eng-{parent.id}', text).", text)
            self.assertIn("You were spawned by another Engineer. Own the bounded assignment in the parent message.", text)
            for forbidden in ["human.", "archive()", "agents.spawn_new_engineer(*", "## Diagrams",
                              "agents.spawn_user_owned_engineer", "Engineers the user manages"]:
                self.assertNotIn(forbidden, text)

    def test_an_advisor_reports_its_analysis(self):
        parent = self.agent(claude(), preset="engineer")
        for runtime in [native("astra", "xhigh"), claude("fable", "xhigh")]:
            advisor = self.agent(runtime, preset="advisor", parent=parent.id)
            text = prompt.instructions(advisor)
            self.assertIn(
                f"Your agent id is adv-{advisor.id}; your parent agent is eng-{parent.id}."
                "\n\nMessages from your parent define your task. Report to your parent with "
                f"mail.send('eng-{parent.id}', text)."
                f"\n\nComplete your independent analysis and send it to your parent with mail.send('eng-{parent.id}', text). "
                "Ask a known agent for context the same way, and end your turn when blocked on a reply.\n",
                text,
            )
            self.assertIn("[message from <agent id>]", text)
            self.assertEqual(text.count("the user can read it one level down and never gets a card from it."), 1)
            self.assertNotIn("agents.spawn_user_owned_engineer", text)

    def test_only_agents_the_user_manages_may_start_engineers_for_the_user(self):
        signature = "agents.spawn_user_owned_engineer(*, task_name:"
        direct = self.agent(claude(), preset="engineer")
        owned = self.agent(claude(), preset="engineer", started_by=direct.id)
        for agent in [direct, owned]:
            text = prompt.instructions(agent)
            self.assertIn(signature, text)
            self.assertLess(text.index("# Engineers\n"), text.index(signature))
            self.assertIn("chat.send(text, kind, *, thread)", text)
            self.assertNotIn("mail.send", text)
        note = (f"Engineer eng-{direct.id} started you for the user; if you need more context, "
                f"ask it with agents.message(agent_id='eng-{direct.id}', message=text). You own the user\'s technical outcome.")
        self.assertIn(note, prompt.instructions(owned))
        self.assertNotIn("started you for the user", prompt.instructions(direct))
        native_text = gpt("engineer", "", template(GPT, "user-owned-engineers"))
        self.assertLess(native_text.index("### Engineers\n"), native_text.index("### Engineers the user manages"))
        self.assertLess(native_text.index("### Engineers the user manages"), native_text.index("### Briefing and integrating work"))

    def test_a_missing_working_directory_still_gets_instructions(self):
        gone = os.path.join(self.dir.name, "gone")
        agent = rho.agent(add_agent(self.state, native(), preset="engineer", cwd=gone))
        with mock.patch.object(fake_rho, "repo_context", side_effect=OSError("no such directory")):
            text = prompt.instructions(agent)
        self.assertIn(f"Working directory: {gone}", text)
        self.assertNotIn("git checkout;", text)
        self.assertNotIn("## Skills", text)

    def test_the_default_policy_lists_the_repo_then_the_user_then_builtin(self):
        def skill(name, root):
            return {"name": name, "description": f"{name} in {root}", "path": f"{root}/{name}/SKILL.md"}

        repo_skills = self.cwd + "/.agents/skills"
        self.state.contexts[self.cwd]["skills"] = [skill("shared", repo_skills), skill("repo-only", repo_skills)]
        self.state.skill_dirs[skills.USER] = [skill("shared", skills.USER), skill("user-only", skills.USER), skill("both", skills.USER)]
        self.state.builtin = [skill("both", "/rho/skills"), skill("builtin-only", "/rho/skills")]
        self.state.agents_files[prompt.USER_AGENTS_FILE] = {"path": prompt.USER_AGENTS_FILE, "content": "User rule."}
        text = prompt.instructions(self.agent(claude(), preset="engineer"))
        catalogue = text[text.index("### Available skills"):]
        for line in ["shared: shared in " + repo_skills, "repo-only: repo-only in " + repo_skills,
                     "user-only: user-only in " + skills.USER, "both: both in " + skills.USER,
                     "builtin-only: builtin-only in /rho/skills"]:
            self.assertIn("- " + line, catalogue)
        self.assertNotIn("shared in " + skills.USER, catalogue)
        self.assertNotIn("both in /rho/skills", catalogue)
        self.assertLess(text.index("User rule."), text.index("Use tabs."))

    def test_merge_keeps_the_first_of_a_name_and_sorts(self):
        first = [{"name": "b", "description": "1", "path": "/1/b/SKILL.md"}]
        second = [{"name": "b", "description": "2", "path": "/2/b/SKILL.md"}, {"name": "a", "description": "2", "path": "/2/a/SKILL.md"}]
        self.assertEqual([(s["name"], s["description"]) for s in skills.merge(first, second)], [("a", "2"), ("b", "1")])
