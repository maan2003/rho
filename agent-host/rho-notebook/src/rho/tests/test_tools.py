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
        self.assertTrue(hasattr(ns["agents"], "spawn_new_engineer"))
        # An Engineer another started has the same tools.
        started = add_agent(self.state, native(), preset="engineer", parent=self.root, cwd=self.tmp)
        ns, _ = self.notebook(started)
        self.assertTrue(hasattr(ns["agents"], "spawn_new_engineer"))
        self.assertFalse(hasattr(ns["agents"], "spawn_user_owned_engineer"))
        advisor = add_agent(self.state, native(), preset="advisor", parent=self.root, cwd=self.tmp)
        ns, _ = self.notebook(advisor)
        self.assertEqual(sorted(ns), ["agents", "chat", "skills"])
        self.assertTrue(hasattr(ns["agents"], "message"))
        self.assertFalse(hasattr(ns["agents"], "spawn_new_engineer"), "an advisor starts no agents")

    def test_spawn_new_engineer(self):
        ns, _ = self.notebook(self.root)
        work = tempfile.mkdtemp()
        printed, reply = run(lambda: ns["agents"].spawn_new_engineer(task_name="fix-it", prompt="Fix it.", workdir=work))
        [child] = [id for id in self.state.agents if id != self.root]
        self.assertEqual(self.state.agents[child], {
            "runtime": native("terra", "xhigh"),
            "props": {"name": "fix-it", "preset": "engineer", "parent": self.root, "cwd": work},
        })
        # The brief goes by mail, outside the chat.
        self.assertEqual(self.state.mail, [(self.root, child, f"[message from eng-{self.root}]\nFix it.")])
        self.assertEqual(self.state.messages, [])
        self.assertEqual(reply, f'Spawned agent eng-{child} for task "fix-it". It is working now in {work}, and it answers whoever your brief named. Follow up with agents.message(agent_id=\'eng-{child}\', message=text).')
        self.assertEqual(printed, reply + "\n")
        self.assertEqual(asyncio.run(_await(reply)), reply)
        # The child holds its brief.
        self.assertEqual([a for a, *_ in self.state.inputs], [child])
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

    def test_an_auto_workset_starts_claude_agents_only(self):
        # The host refuses a native agent in an auto workset, so neither
        # spawn may ask for one, whoever asks.
        with mock.patch.object(fake_rho.STATE, "permissions", "auto"):
            for spawner in [claude("opus", "medium"), claude("fable", "xhigh")]:
                self.assertEqual(tools.engineer_runtime(spawner), claude("opus", "medium"))
                self.assertEqual(tools.advisor_runtime(spawner), claude("fable", "xhigh"))

    def test_spawn_new_advisor_shares_the_askers_directory(self):
        ns, _ = self.notebook(self.root)
        _, reply = run(lambda: ns["agents"].spawn_new_advisor("Is this right?"))
        [adv] = [id for id in self.state.agents if id != self.root]
        self.assertEqual(self.state.agents[adv], {
            "runtime": native("sol", "xhigh"),
            "props": {"name": "advisor", "preset": "advisor", "parent": self.root, "cwd": self.tmp},
        })
        self.assertEqual([to for _, to, _ in self.state.mail], [adv])
        self.assertEqual(reply, f"Advisor adv-{adv} is considering the question. Its answer will arrive as a message from it.")

    def test_an_engineer_another_started_may_reach_the_user_and_its_starter_may_interrupt_it(self):
        ns, _ = self.notebook(self.root)
        run(lambda: ns["agents"].spawn_new_engineer(task_name="side", prompt="Open a thread for the user."))
        [started] = [id for id in self.state.agents if id != self.root]
        sns, _ = self.notebook(started)
        # Its brief may hand it the user: it opens a thread and mentions them.
        _, thread = run(lambda: sns["chat"].start("ops", "Side work starts.", mention="user", kind="fyi"))
        self.assertEqual(self.state.messages[-1]["mentions"], [("user", "fyi")])
        self.assertEqual(self.state.messages[-1]["id"], thread)
        # The Engineer that started it still manages it.
        run(lambda: ns["agents"].cancel(agent_id=f"eng-{started}"))
        self.assertEqual(self.state.calls[-1], ("cancel", (started,)))

    def test_cancel_rules(self):
        ns, _ = self.notebook(self.root)
        child = add_agent(self.state, native(), preset="engineer", parent=self.root, cwd=self.tmp)
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
        a = add_agent(self.state, native(), preset="engineer", parent=self.root, cwd=self.tmp)
        b = add_agent(self.state, native(), preset="engineer", parent=a, cwd=self.tmp)
        ns, _ = self.notebook(b)
        run(lambda: ns["agents"].spawn_new_engineer(task_name="deep", prompt="p"))
        c = next(id for id, spec in self.state.agents.items() if spec["props"].get("name") == "deep")
        ns, _ = self.notebook(c)
        before = len(self.state.agents)
        with self.assertRaisesRegex(RuntimeError, r"spawn depth limit \(3\) reached"):
            ns["agents"].spawn_new_engineer(task_name="deeper", prompt="p")
        self.assertEqual(len(self.state.agents), before)

    def test_a_post_goes_in_the_thread_it_names_and_reaches_whom_it_mentions(self):
        ns, _ = self.notebook(self.root)
        child = add_agent(self.state, native(), preset="engineer", parent=self.root, cwd=self.tmp)
        rho_ = fake_rho.post_user(None, "rho")
        first = fake_rho.post_user(rho_, "Do it.", [self.root])
        other = fake_rho.post_user(rho_, "Something else.", [self.root])
        printed, reply = run(lambda: ns["chat"].post(first, "On it."))
        self.assertEqual((printed, reply), (f"Posted in #{first}.\n", f"Posted in #{first}."))
        printed, reply = run(lambda: ns["chat"].post(first, "Done.", mention="user", kind="result"))
        self.assertEqual((printed, reply), ("", ""))
        run(lambda: ns["chat"].post(other, "Also done.", mention=["user", f"eng-{child}"], kind="ask"))
        posted = [(m["parent"], m["text"], m["mentions"]) for m in self.state.messages if m["sender"] == self.root]
        self.assertEqual(posted, [
            (first, "On it.", []),
            (first, "Done.", [("user", "result")]),
            (other, "Also done.", [("user", "ask"), (child, "ask")]),
        ])
        # A mention reaches an agent outside the thread; posting joins no one.
        self.assertEqual([m["_to"] for m in self.state.messages if m["sender"] == self.root], [set(), set(), {child}])
        self.assertEqual(self.state.members, {})

    def test_chat_rules(self):
        ns, _ = self.notebook(self.root)
        rho_ = fake_rho.post_user(None, "rho")
        thread = fake_rho.post_user(rho_, "Do it.", [self.root])
        chat = ns["chat"]
        with self.assertRaisesRegex(ValueError, "needs kind"):
            chat.post(thread, "Done.", mention="user")
        with self.assertRaisesRegex(ValueError, "needs kind"):
            chat.post(thread, "Done.", mention="user", kind="status")
        with self.assertRaisesRegex(ValueError, "goes with a mention"):
            chat.post(thread, "Done.", kind="result")
        with self.assertRaisesRegex(ValueError, "yourself"):
            chat.post(thread, "hi", mention=f"eng-{self.root}", kind="fyi")
        with self.assertRaisesRegex(ValueError, "no thread #99"):
            chat.post(99, "hi")
        with self.assertRaisesRegex(ValueError, "needs text"):
            chat.post(thread, " ")
        child = add_agent(self.state, native(), preset="engineer", parent=self.root, cwd=self.tmp)
        cns, _ = self.notebook(child)
        advisor = add_agent(self.state, native(), preset="advisor", parent=self.root, cwd=self.tmp)
        ans, _ = self.notebook(advisor)
        with self.assertRaisesRegex(ValueError, f"parent agent eng-{self.root}"):
            ans["chat"].post(thread, "Hi.", mention="user", kind="fyi")
        self.assertEqual(len(self.state.messages), 2, "nothing refused was posted")
        # A new subject in a stream that is new: the root is made, the
        # thread goes under it, and its starter joins it; the mentioned
        # child does not.
        printed, started = run(lambda: chat.start("ops", "Look.", mention=f"eng-{child}", kind="ask"))
        self.assertEqual(printed, f"Started #{started}.\n")
        ops = self.state.messages[-2]
        self.assertEqual((ops["parent"], ops["sender"], ops["text"]), (None, self.root, "ops"))
        self.assertEqual(self.state.messages[-1], {"id": started, "at": 0, "parent": ops["id"], "sender": self.root, "mentions": [(child, "ask")], "text": "Look.", "_to": {child}})
        # (The user's mention joined the root to `thread` meanwhile.)
        self.assertEqual(self.state.members, {(thread, self.root): True, (started, self.root): True})
        # The same stream again is the same root.
        _, again = run(lambda: chat.start(" ops ", "More."))
        self.assertEqual(self.state.messages[-1]["parent"], ops["id"])
        self.assertEqual(fake_rho.thread(again)["path"], [(ops["id"], "ops"), (again, "More.")])
        # Agents message each other outside the user's chat, by mail, any
        # agent on the host.
        printed, _ = run(lambda: ns["agents"].message(agent_id=f"eng-{child}", message="Psst."))
        self.assertEqual(printed, f"Sent to eng-{child}.\n")
        run(lambda: cns["agents"].message(agent_id=self.root, message="Yes?"))
        self.assertEqual(self.state.mail, [
            (self.root, child, f"[message from eng-{self.root}]\nPsst."),
            (child, self.root, f"[message from eng-{child}]\nYes?"),
        ])
        self.assertEqual([(a, t, r, m) for a, t, r, m in self.state.inputs if m is None], [
            (child, f"[message from eng-{self.root}]\nPsst.", "tool", None),
            (self.root, f"[message from eng-{child}]\nYes?", "tool", None),
        ])
        with self.assertRaisesRegex(ValueError, "yourself"):
            ns["agents"].message(agent_id=self.root, message="hi")
        with self.assertRaisesRegex(ValueError, "no agent nobody on this host"):
            ns["agents"].message(agent_id="eng-nobody", message="hi")
        # Any prefix that names one agent of the host will do, in any
        # workset; one that names several is refused.
        self.state.elsewhere.update({"farther1", "farthest"})
        with self.assertRaisesRegex(ValueError, "names 2 agents"):
            ns["agents"].message(agent_id="eng-farth", message="Which?")
        printed, _ = run(lambda: ns["agents"].message(agent_id="eng-farthes", message="From another workset."))
        self.assertEqual(printed, "Sent to farthest.\n")
        self.assertEqual(self.state.mail[-1][1], "farthest")
        self.assertEqual(len(self.state.messages), 5, "mail is not chat")

        self.assertFalse(hasattr(chat, "mail"), "chat is the user's")
        # Join and read.
        printed, _ = run(lambda: cns["chat"].join(started))
        self.assertEqual(printed, f"Joined #{started}.\n")
        self.assertEqual(fake_rho.thread(started)["agents"], [self.root, child])
        printed, _ = run(lambda: chat.read(thread))
        self.assertEqual(printed, f"[#{thread} rho › Do it.]\n#{thread} user: Do it.\n")
        printed, _ = run(lambda: chat.read(rho_))
        self.assertEqual(printed, f"[#{rho_} rho]\n#{rho_} user: rho\n#{thread} user: Do it.\n")

    def test_an_agent_on_another_host_is_reached_only_once_its_host_is_approved_and_it_is_accepted(self):
        ns, _ = self.notebook(self.root)
        external = ns["agents"].external
        theirs = "cd" * 32 + "/their-agent"
        printed, mine = run(external.address)
        self.assertEqual(mine, "ab" * 32 + "/" + self.root)
        self.assertEqual(printed, mine + "\n")
        self.assertEqual(run(external.contacts)[0], "You have accepted no address.\n")

        def send(text, awaited=True):
            """A cell that sends, and awaits the send or lets it run."""

            async def cell():
                sending = external.send(address=theirs, message=text)
                if awaited:
                    return await sending
                await asyncio.sleep(0.05)

            return run(lambda: asyncio.run(cell()))

        with self.assertRaisesRegex(RuntimeError, "is not an address you accepted"):
            send("early")
        with self.assertRaisesRegex(ValueError, "has not approved the host " + "cd" * 32):
            external.accept(address=theirs, name="dpc's agent")
        with self.assertRaisesRegex(ValueError, "an address is <endpoint id>/<local id>"):
            external.accept(address="their-agent", name="x")
        self.assertEqual((self.state.contacts, self.state.sent), ({}, []))

        self.state.hosts.add("cd" * 32)
        printed, _ = run(lambda: external.accept(address=f" {theirs} ", name=" dpc's agent "))
        self.assertEqual(
            printed,
            f"Accepted {theirs} as dpc's agent. It reaches you once it has your address, {mine}, "
            "and you reach it once it has accepted that address.\n",
        )
        with self.assertRaisesRegex(ValueError, "already accepted as \"dpc's agent\""):
            external.accept(address=theirs, name="someone else")
        with self.assertRaisesRegex(ValueError, "a message needs text"):
            send("  ")

        # The send ends with what the other host said. It runs whether or
        # not the cell awaits it.
        printed, returned = send("Hello.")
        self.assertEqual((printed, returned), ("Delivered to dpc's agent.\n", "Delivered to dpc's agent."))
        printed, _ = send("And again.", awaited=False)
        self.assertEqual(printed, "Delivered to dpc's agent.\n")
        self.assertEqual(self.state.sent, [(self.root, theirs, "Hello."), (self.root, theirs, "And again.")])
        self.assertEqual(self.state.messages, [], "it is no chat message")
        self.state.refusing.add(theirs)
        with self.assertRaisesRegex(RuntimeError, "refused the message"):
            send("Anyone?")
        self.assertEqual(len(self.state.sent), 2)
        self.assertEqual(run(external.contacts)[0], f"dpc's agent: {theirs}\n")

        # Another agent has accepted nothing, and an Advisor talks to no
        # other host. `agents.message` stays inside this host.
        child = add_agent(self.state, native(), preset="engineer", parent=self.root, cwd=self.tmp)
        theirs_too, _ = self.notebook(child)
        self.assertEqual(run(theirs_too["agents"].external.contacts)[0], "You have accepted no address.\n")
        advisor = add_agent(self.state, native(), preset="advisor", parent=self.root, cwd=self.tmp)
        adv, _ = self.notebook(advisor)
        self.assertFalse(hasattr(adv["agents"], "external"))
        with self.assertRaises((ValueError, RuntimeError)):
            ns["agents"].message(agent_id=theirs, message="Hello.")
        self.assertEqual(len(self.state.sent), 2)

        self.state.endpoint = None
        with self.assertRaisesRegex(ValueError, "no iroh endpoint"):
            external.address()

    def test_leave_and_read_by_any_id(self):
        ns, _ = self.notebook(self.root)
        chat = ns["chat"]
        rho_ = fake_rho.post_user(None, "rho")
        thread = fake_rho.post_user(rho_, "Do it.")
        ids = [fake_rho.post_user(thread, f"m{i}") for i in range(5)]
        run(lambda: chat.join(thread))
        printed, _ = run(lambda: chat.leave(thread))
        self.assertEqual(printed, f"Left #{thread}.\n")
        self.assertEqual(fake_rho.thread(thread)["agents"], [])
        # A thread shows its message and the newest under it.
        printed, _ = run(lambda: chat.read(thread, last=2))
        self.assertEqual(printed, f"[#{thread} rho › Do it.]\n#{thread} user: Do it.\n#{ids[3]} user: m3\n#{ids[4]} user: m4\n")
        # A message with nothing under it shows the messages around it in
        # its thread, marked.
        printed, _ = run(lambda: chat.read(ids[2], last=3))
        self.assertEqual(printed, f"[#{thread} rho › Do it.]\n#{ids[1]} user: m1\n→ #{ids[2]} user: m2\n#{ids[3]} user: m3\n")
        # At the start of a thread the window does not run off its end.
        printed, _ = run(lambda: chat.read(ids[0], last=3))
        self.assertEqual(printed.splitlines()[1:], [f"#{thread} user: Do it.", f"→ #{ids[0]} user: m0", f"#{ids[1]} user: m1"])
        # The user moved one away, under another stream's thread, and named
        # the old thread.
        ops = fake_rho.post_user(None, "ops")
        aside = fake_rho.post_user(ops, "Aside.")
        fake_rho.reparent("user", ids[4], aside)
        fake_rho.name("user", thread, "Done it")
        printed, _ = run(lambda: chat.read(ids[4], last=1))
        self.assertEqual(printed, f"[#{aside} ops › Aside.]\n→ #{ids[4]} user: m4\n")
        printed, _ = run(lambda: chat.read(thread, last=1))
        self.assertEqual(printed, f"[#{thread} rho › Done it]\n#{thread} user: Do it.\n#{ids[3]} user: m3\n")
        with self.assertRaisesRegex(ValueError, "names no message"):
            chat.read(999)

    def test_roots_lists_the_streams(self):
        ns, _ = self.notebook(self.root)
        chat = ns["chat"]
        printed, _ = run(lambda: chat.roots())
        self.assertEqual(printed, "No threads yet.\n")
        rho_ = fake_rho.post_user(None, "rho")
        ops = fake_rho.post_user(None, "ops")
        fake_rho.name("user", ops, "Operations")
        printed, _ = run(lambda: chat.roots())
        self.assertEqual(printed, f"#{rho_} rho\n#{ops} Operations\n")

    def test_an_agent_names_and_moves_only_in_its_threads(self):
        ns, _ = self.notebook(self.root)
        chat = ns["chat"]
        rho_ = fake_rho.post_user(None, "rho")
        thread = fake_rho.post_user(rho_, "Do it.")
        ids = [fake_rho.post_user(thread, f"m{i}") for i in range(3)]
        ops = fake_rho.post_user(None, "ops")
        with self.assertRaisesRegex(ValueError, "join it first"):
            chat.name(thread, "Did it")
        with self.assertRaisesRegex(ValueError, "join it first"):
            chat.move(ids[1], ops)
        run(lambda: chat.join(thread))
        printed, _ = run(lambda: chat.name(thread, "Did it"))
        self.assertEqual(printed, f"Named #{thread}.\n")
        self.assertEqual(fake_rho.thread(thread)["name"], "Did it")
        # A move takes the message and what hangs under it.
        under = fake_rho.post_user(ids[1], "under m1")
        printed, _ = run(lambda: chat.move(ids[1], ops))
        self.assertEqual(printed, f"Moved #{ids[1]} to #{ops}.\n")
        self.assertEqual([m["parent"] for m in self.state.messages if m["id"] in ids], [thread, ops, thread])
        self.assertEqual(fake_rho.thread(under)["path"], [(ops, "ops"), (ids[1], "m1"), (under, "under m1")])
        # The agent is not in `ops`, so it moves nothing out of it.
        with self.assertRaisesRegex(ValueError, "join it first"):
            chat.move(ids[1], thread)
        with self.assertRaisesRegex(ValueError, "no message #999"):
            chat.move(999, thread)
        with self.assertRaisesRegex(ValueError, "no thread #999"):
            chat.move(ids[0], 999)

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

    def test_a_failed_child_tells_its_parent_by_mail(self):
        ns, _ = self.notebook(self.root)
        run(lambda: ns["agents"].spawn_new_engineer(task_name="fix-it", prompt="Fix it."))
        [child] = [id for id in self.state.agents if id != self.root]
        rho._hook("on_turn_end", child, {"reason": "end_turn", "error": None}, timeout=5)
        rho._hook("on_turn_end", self.root, {"reason": "failed", "error": "boom"}, timeout=5)
        self.assertEqual(len(self.state.mail), 1)
        rho._hook("on_turn_end", child, {"reason": "failed", "error": "quota"}, timeout=5)
        self.assertEqual(self.state.mail[1:], [(child, self.root, f"[message from eng-{child}]\nAgent hit an error and stopped: quota")])
        self.assertEqual(self.state.messages, [])


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
