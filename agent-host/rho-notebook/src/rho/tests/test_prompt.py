"""The default instructions, against the templates and expectations of the
Rust prompt they replace (agent-host/rho-agent/src/prompt.rs at d03660c6)."""

import hashlib
import os
import pathlib
import tempfile
import unittest
from unittest import mock

from . import fake_rho
from .support import add_agent, claude, native

import rho
from rho import prompt, workset
from rho.prompt import render, template

GPT, CLAUDE = "gpt", "claude"

#: Every change from the Rust templates: the model talks through `mail`.
CHANGES = [
    ('human.send and continue', 'mail.send and continue'),
    ('human.send(text, *, kind)  Send the user a message.', 'mail.send(to, text, *, kind=None)  Send mail to "user" or to an agent. Mail to the user needs a kind.'),
    ('Every send has a kind.', 'Every mail to the user has a kind.'),
    ('human.send\nrejects a sentence', 'mail.send\nrejects one with a sentence'),
    ('it reports back to you with agents.message,', 'it reports back to you with mail.send,'),
    ('Use `agents.message` to send findings', 'Use `mail.send` to send findings'),
    ('agents.message(*, agent_id: str, message: str) -> Awaitable[str]', 'mail.send(to: str, text: str) -> Awaitable[str]'),
    ('send it with agents.message, not your code', 'send it with mail.send, not your code'),
    ('agents.message(*, agent_id: str, message: str) → Awaitable[str]', 'mail.send(to: str, text: str) → Awaitable[str]'),
    ('through `agents.message`.', 'through `mail.send`.'),
    ('Use `agents.message` when a known agent', 'Use `mail.send` when a known agent'),
    ('result with agents.message —', 'result with mail.send —'),
]

#: sha256 of each Rust template at d03660c6.
ORIGINALS = {'gpt/engineer.md': 'b95b0d3597d32f0ae31ebb7b1aff7f3c72ed4956f0b7242dc4fa55b1ffb1f9a5', 'gpt/child.md': '849c8f749882a4642fd3f5943507734b08161ed6b463cddef113a88fcbdca537', 'gpt/advisor.md': '3534be0d9c5628b649eb23a0530ee0903278d6f5f791106cad64598ab45cd720', 'gpt/user-owned-engineers.md': '4bf33e7df786cb3d63d38bbb182f221a8fef6cbe20a65c4e25bcc106adb829fb', 'claude/engineer.md': 'e43977d5337491c00a1e5ffc3039f6951dbe05b4f06391722152d2e02266048f', 'claude/child.md': 'fe42931dee099b3527203cef70496fb0f5cc1e45e3f6cae872cf49fb02ce0b4d', 'claude/advisor.md': '240a8976207fbafe5137d3a86479c8fccd4e83d8c595bebd758dba00f7b81c8e', 'claude/user-owned-engineers.md': '1beca8648638fdcef902d2de222d603e789667c1731c1eb694c089fda5ff3d3b'}


def gpt(name, team="", owned="", context=""):
    return render(template(GPT, name), team, owned, context)


def claude_t(name, team="", owned="", context=""):
    return render(template(CLAUDE, name), team, owned, context)


def between(text, start, end):
    return text.split(start, 1)[1].split(end, 1)[0]


class Templates(unittest.TestCase):
    def test_templates_differ_from_the_rust_ones_only_by_the_mail_changes(self):
        root = pathlib.Path(prompt.__file__).with_name("prompt")
        for name, digest in ORIGINALS.items():
            text = (root / name).read_text(encoding="utf-8")
            for old, new in reversed(CHANGES):
                text = text.replace(new, old)
            self.assertEqual(hashlib.sha256(text.encode()).hexdigest(), digest, name)
            for gone in ["human.", "agents.message", "message_agent"]:
                self.assertNotIn(gone, (root / name).read_text(encoding="utf-8"), name)

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
        self.assertLess(collaboration.index("mail.send(to: str, text: str)"), collaboration.index("### Briefing and integrating work"))
        for needle in ["agents.spawn_new_advisor(msg: str)", "agents.spawn_new_engineer(*, task_name:",
                       "task_name is a short kebab-case label", "agents.cancel(",
                       "loads applicable AGENTS.md guidance and the skill catalogue",
                       "Without an explicit request, do NOT consult the Advisor for:"]:
            self.assertIn(needle, collaboration)
        self.assertIn('mail.send(to, text, *, kind=None)  Send mail to "user" or to an agent. Mail to the user needs a kind.', text)
        self.assertIn("Every mail to the user has a kind.", text)
        self.assertIn("answer briefly with mail.send and continue the work", text)
        self.assertIn("(ASD-STE100). mail.send\nrejects one with a sentence of more than 25 words", text)
        self.assertIn("it reports back to you with mail.send, which arrives as agent mail", text)
        self.assertIn("Task.from_session_id(session_id: int) → Task", text)
        self.assertIn("def papercut(", text)
        self.assertTrue(text.endswith("WORKSPACE_SENTINEL"))
        for gone in ["human.", "agents.message", "transcript", "Python Code Mode", "class agents:"]:
            self.assertNotIn(gone, text)

    def test_advisor_prompt_has_only_its_capabilities(self):
        text = gpt("advisor", "TEAM_SENTINEL\n\n")
        self.assertIn("## Working with other agents\n\nTEAM_SENTINEL\n\n", text)
        self.assertIn("focused result with mail.send", text)
        self.assertIn("mail.send(to: str, text: str) → Awaitable[str]", text)
        self.assertIn("Use `mail.send` when a known agent has context", text)
        for forbidden in ["spawn_new_advisor", "spawn_new_engineer", "agents.cancel(*", "human.",
                          "archive()", "## Diagrams", "agents.message"]:
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
        for forbidden in ["human.", "archive()", "spawn_new_advisor", "spawn_new_engineer", "agents.message"]:
            self.assertNotIn(forbidden, text)

    def test_engineer_delegation_rules_in_both_families(self):
        for text in [gpt("engineer"), claude_t("engineer")]:
            self.assertIn(" Engineers\n\n```python\nagents.spawn_new_engineer(", text)
            self.assertLess(text.index("agents.spawn_new_engineer("), text.index("When to use an Engineer:"))
            communication = text.split("# Working with other agents", 1)[1]
            for rule in ["send findings, questions, or a scoped next action", "say whether another reply is needed",
                         "Stop exchanging messages when\nthe requested work is complete",
                         "do not create acknowledgment loops", "Use `mail.send` to send findings"]:
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
        agent = self.agent(native(), preset="engineer", user_facing="true")
        team = (
            f"You are the primary agent in a team of agents collaborating to fulfill the user\'s goals. Your agent id is eng-{agent.id}."
            "\n\nYou were started directly and own the user\'s technical outcome."
            "\n\nYou will receive agent messages in this format:\n```\nMessage Type: MESSAGE\nSender: <agent id>\nPayload:\n<payload text>\n```"
            "\n\nUse `mail.send` for bidirectional communication with any known agent.\n\n"
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
            self.assertIn(f"Your agent id is eng-{child.id}; your parent agent is eng-{parent.id}.\n\nMessages from your parent define your task. Report to your parent with mail.send.", text)
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
                "\n\nMessages from your parent define your task. Report to your parent with mail.send."
                "\n\nComplete your independent analysis and send it to your parent with mail.send. "
                "Ask a known agent for context the same way, and end your turn when blocked on a reply.\n",
                text,
            )
            self.assertNotIn("You will receive agent messages", text)
            self.assertNotIn("agents.spawn_user_owned_engineer", text)

    def test_only_agents_the_user_manages_may_start_engineers_for_the_user(self):
        signature = "agents.spawn_user_owned_engineer(*, task_name:"
        direct = self.agent(claude(), preset="engineer", user_facing="true")
        owned = self.agent(claude(), preset="engineer", user_facing="true", started_by=direct.id)
        for agent in [direct, owned]:
            text = prompt.instructions(agent)
            self.assertIn(signature, text)
            self.assertLess(text.index("# Engineers\n"), text.index(signature))
            self.assertIn("mail.send(to, text, *, kind=None)", text)
        note = (f"Engineer eng-{direct.id} started you for the user; ask it with mail.send if you need "
                "more context. You own the user\'s technical outcome.")
        self.assertIn(note, prompt.instructions(owned))
        self.assertNotIn("started you for the user", prompt.instructions(direct))
        native_text = gpt("engineer", "", template(GPT, "user-owned-engineers"))
        self.assertLess(native_text.index("### Engineers\n"), native_text.index("### Engineers the user manages"))
        self.assertLess(native_text.index("### Engineers the user manages"), native_text.index("### Briefing and integrating work"))

    def test_a_missing_working_directory_still_gets_instructions(self):
        gone = os.path.join(self.dir.name, "gone")
        agent = rho.agent(add_agent(self.state, native(), preset="engineer", cwd=gone))
        with mock.patch.object(fake_rho, "context", side_effect=OSError("no such directory")):
            text = prompt.instructions(agent)
        self.assertIn(f"Working directory: {gone}", text)
        self.assertNotIn("git checkout;", text)
        self.assertNotIn("## Skills", text)
