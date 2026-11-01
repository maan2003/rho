"""The default notebook tools, compaction and turn-end policy, and
`new_agent`."""

import asyncio
import io
import os
import tempfile
from contextlib import redirect_stdout
from unittest import mock

from . import fake_rho
from .support import WorksetCase, add_agent, claude, native, wait_for

import rho
from rho import default, skills, tools, workset


def run(call):
    """A tool call as a cell makes it: what it prints, and what it returns."""
    out = io.StringIO()
    with redirect_stdout(out):
        result = call()
    return out.getvalue(), result


class Tools(WorksetCase):
    def seed(self, state):
        self.tmp = tempfile.mkdtemp()
        self.root = add_agent(state, native("terra", "xhigh"), preset="engineer", cwd=self.tmp)

    def notebook(self, agent_id):
        ns = {}
        here = os.getcwd()
        try:
            rho._on_notebook(agent_id, ns)
            cwd = os.getcwd()
        finally:
            os.chdir(here)
        return ns, cwd

    def test_on_notebook_gives_tools_by_preset_and_enters_the_directory(self):
        ns, cwd = self.notebook(self.root)
        self.assertEqual(cwd, os.path.realpath(self.tmp))
        self.assertEqual(sorted(ns), ["agents", "chat", "skills"])
        self.assertTrue(hasattr(ns["agents"], "spawn_user_owned_engineer"))
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        ns, _ = self.notebook(child)
        self.assertFalse(hasattr(ns["agents"], "spawn_user_owned_engineer"))
        self.assertTrue(hasattr(ns["agents"], "spawn_new_engineer"))
        advisor = add_agent(self.state, native(), preset="advisor", parent=self.root, cwd=self.tmp)
        ns, _ = self.notebook(advisor)
        self.assertEqual(sorted(ns), ["chat", "skills"])

    def test_spawn_new_engineer(self):
        ns, _ = self.notebook(self.root)
        work = tempfile.mkdtemp()
        printed, reply = run(lambda: ns["agents"].spawn_new_engineer(task_name="fix-it", prompt="Fix it.", workdir=work))
        [child] = [id for id in self.state.agents if id != self.root]
        [topic] = self.state.topics
        self.assertEqual(self.state.agents[child], {
            "runtime": native("terra", "xhigh"),
            "props": {"name": "fix-it", "preset": "child", "parent": self.root, "cwd": work},
        })
        # The brief goes in their direct conversation.
        self.assertEqual(self.state.topics[topic], {"id": topic, "name": None, "stream": None, "agents": sorted([self.root, child])})
        [brief] = self.state.messages
        self.assertEqual((brief["topic"], brief["sender"], brief["text"], brief["mentions"]), (topic, self.root, "Fix it.", []))
        self.assertEqual(reply, f'Spawned agent eng-{child} for task "fix-it". It is working now; its results will arrive in your direct conversation #{topic}. It works in {work}. Post in #{topic} to follow up.')
        self.assertEqual(printed, reply + "\n")
        self.assertEqual(asyncio.run(_await(reply)), reply)
        # The child gets its brief through delivery.
        wait_for(lambda: any(a == child for a, *_ in self.state.inputs), "the brief")
        with self.assertRaisesRegex(ValueError, "absolute"):
            ns["agents"].spawn_new_engineer(task_name="x", prompt="p", workdir="relative")
        with self.assertRaisesRegex(ValueError, "not a directory"):
            ns["agents"].spawn_new_engineer(task_name="x", prompt="p", workdir=work + "/missing")
        with self.assertRaisesRegex(ValueError, "prompt must not be empty"):
            ns["agents"].spawn_new_engineer(task_name="x", prompt="  ")

    def test_child_and_advisor_runtimes_follow_the_spawner(self):
        cases = [
            (native("sol", "high"), native("sol", "high"), native("astra", "xhigh")),
            (native("terra", "xhigh"), native("terra", "xhigh"), native("sol", "xhigh")),
            (native("astra", "medium"), native("sol", "high"), claude("fable", "xhigh")),
            (claude("opus", "medium"), claude("opus", "medium"), native("astra", "xhigh")),
            (claude("fable", "medium"), claude("opus", "medium"), native("astra", "xhigh")),
        ]
        for spawner, engineer, advisor in cases:
            self.assertEqual(tools.engineer_runtime(spawner), engineer, spawner)
            self.assertEqual(tools.advisor_runtime(spawner), advisor, spawner)

    def test_spawn_new_advisor_shares_the_askers_directory(self):
        ns, _ = self.notebook(self.root)
        _, reply = run(lambda: ns["agents"].spawn_new_advisor("Is this right?"))
        [adv] = [id for id in self.state.agents if id != self.root]
        [topic] = self.state.topics
        self.assertEqual(self.state.agents[adv], {
            "runtime": native("sol", "xhigh"),
            "props": {"name": "advisor", "preset": "advisor", "parent": self.root, "cwd": self.tmp},
        })
        self.assertEqual(self.state.topics[topic]["agents"], sorted([self.root, adv]))
        self.assertEqual(reply, f"Advisor adv-{adv} is considering the question. Its answer will arrive in your direct conversation #{topic}.")

    def test_spawn_user_owned_engineer(self):
        ns, _ = self.notebook(self.root)
        with self.assertRaisesRegex(ValueError, "pass stream="):
            ns["agents"].spawn_user_owned_engineer(task_name="side", prompt="Do the side thing.")
        # The spawner was started from a message in stream "ops".
        _, origin = fake_rho.start_user_topic("ops", "Work", "Work.")
        props = self.state.agents[self.root]["props"]
        props["origin"] = f"chat:{origin}"
        _, reply = run(lambda: ns["agents"].spawn_user_owned_engineer(task_name="side", prompt="Do the side thing."))
        [owned] = [id for id in self.state.agents if id != self.root]
        brief = self.state.messages[-1]
        self.assertEqual(self.state.topics[brief["topic"]]["stream"], "ops")
        self.assertEqual(self.state.topics[brief["topic"]]["name"], "side")
        self.assertEqual((brief["sender"], brief["text"]), (self.root, "Do the side thing."))
        self.assertEqual(self.state.agents[owned]["props"], {
            "name": "side", "preset": "engineer", "started_by": self.root, "cwd": self.tmp, "origin": f"chat:{brief['id']}",
        })
        # The user learns of it by a mention that makes no card.
        self.assertEqual(brief["mentions"], [("user", "fyi")])
        self.assertIn("It reports to the user, not to you.", reply)
        # It joins the brief's topic and starts from the brief; the spawner
        # does not join.
        wait_for(lambda: any(a == owned for a, *_ in self.state.inputs), "the brief")
        self.assertEqual([m for a, _, _, m in self.state.inputs if a == owned], [brief["id"]])
        self.assertEqual(self.state.topics[brief["topic"]]["agents"], [owned])
        # A stream given wins.
        run(lambda: ns["agents"].spawn_user_owned_engineer(task_name="other", prompt="Other.", stream="side"))
        self.assertEqual(self.state.topics[self.state.messages[-1]["topic"]]["stream"], "side")
        # Its starter may not interrupt it; its own child may.
        with self.assertRaisesRegex(ValueError, "managed by the user"):
            ns["agents"].cancel(agent_id=f"eng-{owned}")
        grandchild = add_agent(self.state, native(), preset="child", parent=owned, cwd=self.tmp)
        gns, _ = self.notebook(grandchild)
        _, reply = run(lambda: gns["agents"].cancel(agent_id=owned))
        self.assertEqual(self.state.calls[-1], ("cancel", (owned,)))
        self.assertEqual(reply, f"Engineer eng-{owned} interrupted. It remains available for follow-up messages.")

    def test_cancel_rules(self):
        ns, _ = self.notebook(self.root)
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        adv = add_agent(self.state, native(), preset="advisor", parent=self.root, cwd=self.tmp)
        with self.assertRaisesRegex(ValueError, "not an Engineer"):
            ns["agents"].cancel(agent_id=adv)
        with self.assertRaisesRegex(ValueError, "yourself"):
            ns["agents"].cancel(agent_id=self.root)
        with self.assertRaisesRegex(ValueError, "role prefix differs"):
            ns["agents"].cancel(agent_id=f"adv-{child}")
        run(lambda: ns["agents"].cancel(agent_id=f"eng-{child}"))
        self.assertEqual(self.state.calls[-1], ("cancel", (child,)))

    def test_spawn_depth_is_limited(self):
        a = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        b = add_agent(self.state, native(), preset="child", parent=a, cwd=self.tmp)
        ns, _ = self.notebook(b)
        run(lambda: ns["agents"].spawn_new_engineer(task_name="deep", prompt="p"))
        c = next(id for id, spec in self.state.agents.items() if spec["props"].get("name") == "deep")
        ns, _ = self.notebook(c)
        before = len(self.state.agents)
        with self.assertRaisesRegex(RuntimeError, r"spawn depth limit \(3\) reached"):
            ns["agents"].spawn_new_engineer(task_name="deeper", prompt="p")
        self.assertEqual(len(self.state.agents), before)

    def test_a_post_goes_in_the_topic_it_names_and_reaches_whom_it_mentions(self):
        ns, _ = self.notebook(self.root)
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        first, _ = fake_rho.start_user_topic("rho", "Do it", "Do it.", [self.root])
        other, _ = fake_rho.start_user_topic("rho", "Else", "Something else.", [self.root])
        printed, reply = run(lambda: ns["chat"].post(first, "On it."))
        self.assertEqual((printed, reply), (f"Posted in #{first}.\n", f"Posted in #{first}."))
        printed, reply = run(lambda: ns["chat"].post(first, "Done.", mention="user", kind="result"))
        self.assertEqual((printed, reply), ("", ""))
        run(lambda: ns["chat"].post(other, "Also done.", mention=["user", f"eng-{child}"], kind="ask"))
        posted = [(m["topic"], m["text"], m["mentions"]) for m in self.state.messages if m["sender"] == self.root]
        self.assertEqual(posted, [
            (first, "On it.", []),
            (first, "Done.", [("user", "result")]),
            (other, "Also done.", [("user", "ask"), (child, "ask")]),
        ])
        # A mention reaches an agent outside the topic; posting joins no one.
        self.assertEqual([m["_to"] for m in self.state.messages if m["sender"] == self.root], [set(), set(), {child}])
        self.assertEqual(self.state.topics[first]["agents"], [])

    def test_chat_rules(self):
        ns, _ = self.notebook(self.root)
        topic, _ = fake_rho.start_user_topic("rho", "Do it", "Do it.", [self.root])
        chat = ns["chat"]
        with self.assertRaisesRegex(ValueError, "needs kind"):
            chat.post(topic, "Done.", mention="user")
        with self.assertRaisesRegex(ValueError, "needs kind"):
            chat.post(topic, "Done.", mention="user", kind="status")
        with self.assertRaisesRegex(ValueError, "goes with a mention"):
            chat.post(topic, "Done.", kind="result")
        with self.assertRaisesRegex(ValueError, "yourself"):
            chat.post(topic, "hi", mention=f"eng-{self.root}", kind="fyi")
        with self.assertRaisesRegex(ValueError, "no topic 99"):
            chat.post(99, "hi")
        with self.assertRaisesRegex(ValueError, "needs text"):
            chat.post(topic, " ")
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        cns, _ = self.notebook(child)
        with self.assertRaisesRegex(ValueError, f"parent agent eng-{self.root}"):
            cns["chat"].post(topic, "Hi.", mention="user", kind="fyi")
        self.assertEqual(len(self.state.messages), 1, "nothing refused was posted")
        printed, started = run(lambda: chat.start("ops", "side", "Look.", mention=f"eng-{child}", kind="ask"))
        self.assertEqual(printed, f"Started topic #{started}.\n")
        # Its starter joins it; the mentioned child does not.
        self.assertEqual(self.state.topics[started], {"id": started, "name": "side", "stream": "ops", "agents": [self.root]})
        self.assertEqual(self.state.messages[-1]["_to"], {child})
        # Direct conversations.
        printed, conversation = run(lambda: chat.direct(f"eng-{child}", "Psst."))
        self.assertEqual(printed, f"Posted to eng-{child} in #{conversation}.\n")
        self.assertEqual(run(lambda: cns["chat"].direct(self.root, "Yes?"))[1], conversation)
        self.assertEqual([m["topic"] for m in self.state.messages[-2:]], [conversation, conversation])
        with self.assertRaisesRegex(ValueError, "no one joins"):
            chat.join(conversation)
        with self.assertRaisesRegex(ValueError, "yourself"):
            chat.direct(self.root, "hi")
        # Join and read.
        printed, _ = run(lambda: cns["chat"].join(started))
        self.assertEqual(printed, f"Joined #{started}.\n")
        self.assertEqual(self.state.topics[started]["agents"], [self.root, child])
        printed, _ = run(lambda: cns["chat"].read(conversation, last=1))
        self.assertEqual(printed, f"[#{conversation} direct with eng-{self.root}]\n#{self.state.messages[-1]['id']} eng-{child}: Yes?\n")
        printed, _ = run(lambda: chat.read(topic))
        self.assertEqual(printed, f"[#{topic} rho › Do it]\n#{topic + 1} user: Do it.\n")

    def test_skills_load_keeps_only_skills_that_ask_and_compaction_rereads_them(self):
        ns, _ = self.notebook(self.root)
        keep = os.path.join(self.tmp, "review")
        once = os.path.join(self.tmp, "once")
        for directory, front, body in [
            (keep, "metadata:\n  author: me\n  rho-keep-after-compaction: true\n", "Verdict v1."),
            (once, "metadata:\n  rho-keep-after-compaction: yes\n", "Shown once."),
        ]:
            os.makedirs(directory)
            with open(os.path.join(directory, "SKILL.md"), "w") as file:
                file.write(f"---\nname: x\n{front}---\n{body}\n\n")
        printed, _ = run(lambda: ns["skills"].load(os.path.join(keep, "SKILL.md")))
        self.assertEqual(printed, f'<skill path="{keep}">\n---\nname: x\nmetadata:\n  author: me\n  rho-keep-after-compaction: true\n---\nVerdict v1.\n</skill>\n')
        run(lambda: ns["skills"].load(once))
        run(lambda: ns["skills"].load(keep))
        self.assertEqual(self.state.agents[self.root]["props"]["skills"], keep)
        with open(os.path.join(keep, "SKILL.md"), "w") as file:
            file.write("Verdict v2.\n")
        self.assertEqual(rho._hook("on_compaction", self.root, timeout=5), [f'Your loaded skills, as they are now. Keep following them:\n\n<skill path="{keep}">\nVerdict v2.\n</skill>'])
        with self.assertRaisesRegex(ValueError, "read .*missing/SKILL.md"):
            ns["skills"].load(os.path.join(self.tmp, "missing"))

    def test_keep_after_compaction_matches_the_rust_rules(self):
        def keeps(front):
            return skills.keeps_after_compaction(f"---\nname: a\ndescription: b\n{front}---\nBody.\n")

        self.assertTrue(keeps('metadata:\n  rho-keep-after-compaction: "true"\n'))
        self.assertTrue(keeps("metadata:\n  author: me\n  rho-keep-after-compaction: true\n"))
        self.assertFalse(keeps(""))
        self.assertFalse(keeps('rho-keep-after-compaction: "true"\n'))
        self.assertFalse(keeps('metadata:\n  rho-keep-after-compaction: "false"\n'))
        self.assertFalse(keeps('metadata:\n  rho-keep-after-compaction: "yes"\n'))
        self.assertFalse(keeps("metadata:\n  rho-keep-after-compaction: yes\n"))
        self.assertFalse(skills.keeps_after_compaction("rho-keep-after-compaction: true\n"))
        self.assertTrue(skills.keeps_after_compaction("\ufeff---\r\nmetadata:\r\n  rho-keep-after-compaction: true\r\n---  \r\nBody."))

    def test_a_failed_child_tells_its_parent_in_their_direct_conversation(self):
        ns, _ = self.notebook(self.root)
        run(lambda: ns["agents"].spawn_new_engineer(task_name="fix-it", prompt="Fix it."))
        [child] = [id for id in self.state.agents if id != self.root]
        [topic] = self.state.topics
        rho._hook("on_turn_end", child, {"reason": "end_turn", "error": None}, timeout=5)
        rho._hook("on_turn_end", self.root, {"reason": "failed", "error": "boom"}, timeout=5)
        self.assertEqual(len(self.state.messages), 1)
        rho._hook("on_turn_end", child, {"reason": "failed", "error": "quota"}, timeout=5)
        self.assertEqual([(m["topic"], m["sender"], m["text"]) for m in self.state.messages[1:]],
                         [(topic, child, "Agent hit an error and stopped: quota")])


async def _await(reply):
    return await reply


class NewAgent(WorksetCase):
    def setUp(self):
        super().setUp()
        self.dir = tempfile.mkdtemp()
        patcher = mock.patch.object(workset, "root", self.dir)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_with_a_repo_it_clones_and_works_there(self):
        output, error = rho._eval(
            f"print(new_agent(repo=\'https://github.com/org/proj.git\', rev=\'abc\', runtime={claude()!r}))"
        )
        self.assertIsNone(error)
        id = output.strip()
        cwd = os.path.join(self.dir, "proj")
        self.assertEqual(self.state.calls, [("clone", ("https://github.com/org/proj.git", cwd, "abc"))])
        self.assertEqual(self.state.agents[id], {
            "runtime": claude(),
            "props": {"preset": "engineer", "cwd": cwd},
        })
        self.assertEqual(self.state.messages, [])

    def test_ssh_urls_name_the_directory(self):
        rho._eval(f"new_agent(repo=\'git@github.com:org/tool.git\', runtime={native()!r})")
        self.assertEqual(self.state.calls[0][1][1], os.path.join(self.dir, "tool"))

    def test_without_a_repo_it_works_in_cwd_or_the_root(self):
        _, error = rho._eval(f"a = new_agent(runtime={native()!r})\nb = new_agent(cwd={self.dir!r}, runtime={native()!r}, preset=\'advisor\')")
        self.assertIsNone(error)
        a, b = workset.namespace["a"], workset.namespace["b"]
        self.assertEqual(self.state.agents[a]["props"]["cwd"], self.dir)
        self.assertEqual(self.state.agents[b]["props"]["preset"], "advisor")
        _, error = rho._eval(f"new_agent(runtime={native()!r}, preset=\'child\')")
        self.assertIn("preset", error)
        _, error = rho._eval(f"new_agent(cwd=\'relative\', runtime={native()!r})")
        self.assertIn("absolute directory", error)
        _, error = rho._eval(f"new_agent(repo=\'r\', cwd=\'/\', runtime={native()!r})")
        self.assertIn("not both", error)
