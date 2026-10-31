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
        self.root = add_agent(state, native("terra", "xhigh"), preset="engineer", user_facing="true", cwd=self.tmp)

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
        self.assertEqual(sorted(ns), ["agents", "mail", "skills"])
        self.assertTrue(hasattr(ns["agents"], "spawn_user_owned_engineer"))
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        ns, _ = self.notebook(child)
        self.assertFalse(hasattr(ns["agents"], "spawn_user_owned_engineer"))
        self.assertTrue(hasattr(ns["agents"], "spawn_new_engineer"))
        advisor = add_agent(self.state, native(), preset="advisor", parent=self.root, cwd=self.tmp)
        ns, _ = self.notebook(advisor)
        self.assertEqual(sorted(ns), ["mail", "skills"])

    def test_spawn_new_engineer(self):
        ns, _ = self.notebook(self.root)
        work = tempfile.mkdtemp()
        printed, reply = run(lambda: ns["agents"].spawn_new_engineer(task_name="fix-it", prompt="Fix it.", workdir=work))
        [child] = [id for id in self.state.agents if id != self.root]
        self.assertEqual(self.state.agents[child], {
            "runtime": native("terra", "xhigh"),
            "props": {"name": "fix-it", "preset": "child", "parent": self.root, "cwd": work},
        })
        self.assertEqual(self.state.mails[-1]["sender"], self.root)
        self.assertEqual((self.state.mails[-1]["to"], self.state.mails[-1]["text"]), (child, "Fix it."))
        self.assertEqual(reply, f'Spawned agent eng-{child} for task "fix-it". It is working now; its results will arrive as mail from that Engineer. It works in {work}. Use mail.send to follow up.')
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
        self.assertEqual(self.state.agents[adv], {
            "runtime": native("sol", "xhigh"),
            "props": {"name": "advisor", "preset": "advisor", "parent": self.root, "cwd": self.tmp},
        })
        self.assertEqual(reply, f"Advisor adv-{adv} is considering the question. Its answer will arrive as mail.")

    def test_spawn_user_owned_engineer(self):
        ns, _ = self.notebook(self.root)
        _, reply = run(lambda: ns["agents"].spawn_user_owned_engineer(task_name="side", prompt="Do the side thing."))
        [owned] = [id for id in self.state.agents if id != self.root]
        self.assertEqual(self.state.agents[owned]["props"], {
            "name": "side", "preset": "engineer", "started_by": self.root, "user_facing": "true", "cwd": self.tmp,
        })
        self.assertNotIn("parent", self.state.agents[owned]["props"])
        self.assertIn("It appears in the user\'s agent list and reports to the user, not to you.", reply)
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

    def test_mail_rules(self):
        ns, _ = self.notebook(self.root)
        with self.assertRaisesRegex(ValueError, "needs kind"):
            ns["mail"].send("user", "Done.")
        printed, _ = run(lambda: ns["mail"].send("user", "Done.", kind="result"))
        self.assertEqual(printed, "")
        self.assertEqual(self.state.mails[-1], {"id": 1, "at": 0, "sender": self.root, "to": "user", "re": None, "kind": "result", "text": "Done."})
        with self.assertRaisesRegex(ValueError, "yourself"):
            ns["mail"].send(f"eng-{self.root}", "hi")
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        printed, reply = run(lambda: ns["mail"].send(f"eng-{child}", "More."))
        self.assertEqual(reply, f"Message sent to eng-{child}.")
        self.assertEqual(self.state.mails[-1]["to"], child)
        cns, _ = self.notebook(child)
        with self.assertRaisesRegex(ValueError, f"parent agent eng-{self.root}"):
            cns["mail"].send("user", "Hi.", kind="fyi")
        with self.assertRaisesRegex(ValueError, "needs text"):
            cns["mail"].send(self.root, " ")

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
        self.assertEqual(rho._hook("on_compaction", self.root, timeout=5), [f'<skill path="{keep}">\nVerdict v2.\n</skill>'])
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

    def test_a_failed_child_tells_its_parent(self):
        child = add_agent(self.state, native(), preset="child", parent=self.root, cwd=self.tmp)
        rho._hook("on_turn_end", child, {"reason": "end_turn", "error": None}, timeout=5)
        rho._hook("on_turn_end", self.root, {"reason": "failed", "error": "boom"}, timeout=5)
        self.assertEqual(self.state.mails, [])
        rho._hook("on_turn_end", child, {"reason": "failed", "error": "quota"}, timeout=5)
        self.assertEqual([(m["sender"], m["to"], m["text"]) for m in self.state.mails],
                         [(child, self.root, "Agent hit an error and stopped: quota")])


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
            "props": {"preset": "engineer", "user_facing": "true", "cwd": cwd},
        })
        self.assertEqual(self.state.mails, [])

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
