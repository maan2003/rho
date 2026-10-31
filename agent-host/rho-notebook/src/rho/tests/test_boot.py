"""The boot contract: `_main`, `_eval`, `_hook` and `_on_notebook`."""

import asyncio
import os
import threading
import unittest

from .support import Workset, WorksetCase, add_agent, native, wait_for

import rho
from rho import default, hooks, prompt, workset


class Eval(WorksetCase):
    def test_captures_output_and_keeps_the_namespace(self):
        self.assertEqual(rho._eval("x = 6\nprint(x * 7)"), ("42\n", None))
        self.assertEqual(rho._eval("import sys\nprint(x, end=\'\')\nprint(\'warn\', file=sys.stderr)"), ("6warn\n", None))

    def test_top_level_await_runs_on_the_workset_loop(self):
        output, error = rho._eval(
            "import asyncio, threading\nawait asyncio.sleep(0)\nprint(threading.current_thread() is not threading.main_thread())"
        )
        self.assertEqual((output, error), ("True\n", None))

    def test_a_raise_returns_the_output_so_far_and_a_traceback_from_the_users_code(self):
        output, error = rho._eval("print(\'before\')\ndef f():\n    return 1 / 0\nf()\nprint(\'after\')")
        self.assertEqual(output, "before\n")
        self.assertTrue(error.startswith("Traceback (most recent call last):\n  File \"<eval-"), error)
        self.assertIn("return 1 / 0", error)
        self.assertTrue(error.rstrip().endswith("ZeroDivisionError: division by zero"), error)
        self.assertNotIn("_boot.py", error)
        self.assertEqual(rho._eval("print(1)"), ("1\n", None))

    def test_system_exit_does_not_end_the_loop(self):
        output, error = rho._eval("raise SystemExit(3)")
        self.assertIn("SystemExit: 3", error)
        self.assertEqual(rho._eval("print(2)"), ("2\n", None))

    def test_output_of_other_threads_is_not_captured(self):
        import io
        import sys

        started, release = threading.Event(), threading.Event()
        workset.namespace["started"], workset.namespace["release"] = started, release
        result = []
        thread = threading.Thread(target=lambda: result.append(rho._eval("import asyncio\nprint(\'mine\')\nstarted.set()\nawait asyncio.to_thread(release.wait)")))
        thread.start()
        started.wait(5)
        captured = io.StringIO()
        outer = sys.stdout.inner
        sys.stdout.inner = captured
        try:
            print("elsewhere")
        finally:
            sys.stdout.inner = outer
        release.set()
        thread.join(5)
        self.assertEqual(result, [("mine\n", None)])
        self.assertEqual(captured.getvalue(), "elsewhere\n")

    def test_the_creation_helper_is_in_the_namespace(self):
        self.assertIs(workset.namespace["new_agent"], default.new_agent)
        self.assertIs(workset.namespace["rho"], rho)


class Hooks(WorksetCase):
    def seed(self, state):
        self.id = add_agent(state, native(), preset="engineer", cwd="/")

    def test_a_raising_hook_raises_in_the_caller_and_the_loop_lives_on(self):
        def broken(agent):
            raise LookupError(f"no prompt for {agent.id}")

        hooks.instructions = broken
        with self.assertRaisesRegex(LookupError, f"no prompt for {self.id}"):
            rho._hook("instructions", self.id, timeout=5)
        self.assertEqual(rho._eval("print(3)"), ("3\n", None))

    def test_an_async_hook_runs_on_the_loop_with_the_agent(self):
        seen = []

        async def instructions(agent):
            await asyncio.sleep(0)
            seen.append((agent, threading.current_thread() is not threading.main_thread()))
            return "be brief"

        hooks.instructions = instructions
        self.assertEqual(rho._hook("instructions", self.id, timeout=5), "be brief")
        self.assertEqual(seen, [(rho.agent(self.id), True)])

    def test_extra_arguments_pass_through(self):
        hooks.on_turn_end = lambda agent, outcome: (agent.id, outcome)
        outcome = {"reason": "end_turn", "error": None}
        self.assertEqual(rho._hook("on_turn_end", self.id, outcome, timeout=5), (self.id, outcome))

    def test_a_slow_async_hook_times_out(self):
        async def slow(agent):
            await asyncio.sleep(30)

        hooks.instructions = slow
        with self.assertRaises(TimeoutError):
            rho._hook("instructions", self.id, timeout=0.05)
        self.assertEqual(rho._eval("print(4)"), ("4\n", None))

    def test_the_default_hook_is_the_default_prompt(self):
        self.assertIn("You are Rho", rho._hook("instructions", self.id, timeout=5))

    def test_on_notebook_runs_on_the_calling_thread(self):
        here = os.getcwd()
        self.addCleanup(os.chdir, here)
        threads = []

        async def on_notebook(agent, ns):
            threads.append(threading.get_ident())
            ns["who"] = agent.id

        hooks.on_notebook = on_notebook
        ns = {}
        rho._on_notebook(self.id, ns)
        self.assertEqual((ns, threads), ({"who": self.id}, [threading.get_ident()]))


class Init(unittest.TestCase):
    def start(self, init, seed=None):
        ws = Workset(init, seed)
        self.addCleanup(ws.stop)
        return ws

    def test_init_replaces_policy(self):
        ws = self.start("import rho\nrho.hooks.instructions = lambda agent: \'custom\'\nmotto = \'hi\'\n")
        self.assertEqual(ws.ready, [None])
        self.assertEqual(hooks.instructions(None), "custom")
        self.assertEqual(rho._eval("print(motto)"), ("hi\n", None))

    def test_a_failing_init_reports_and_keeps_the_default_policy(self):
        ws = self.start(
            "import rho\nrho.hooks.instructions = lambda agent: \'custom\'\nrho.hooks.on_agent = None\nmotto = \'hi\'\nraise RuntimeError(\'bad init\')\n",
            lambda state: add_agent(state, native(), preset="engineer", cwd="/"),
        )
        [error] = ws.ready
        self.assertIn("init.py", error)
        self.assertIn("RuntimeError: bad init", error)
        self.assertIs(hooks.instructions, prompt.instructions)
        self.assertIs(hooks.on_agent, default.on_agent)
        self.assertNotIn("motto", workset.namespace)
        self.assertIn("new_agent", workset.namespace)
        # The default delivery runs for the existing agent.
        from . import fake_rho

        fake_rho.post_user_mail("agent1", "hello")
        wait_for(lambda: ws.state.inputs, "delivery")

    def test_init_may_await(self):
        ws = self.start("import asyncio\nawait asyncio.sleep(0)\nawaited = True\n")
        self.assertEqual(ws.ready, [None])
        self.assertTrue(workset.namespace["awaited"])

    def test_no_init_file(self):
        ws = self.start(None)
        self.assertEqual(ws.ready, [None])
        self.assertIs(hooks.instructions, prompt.instructions)
