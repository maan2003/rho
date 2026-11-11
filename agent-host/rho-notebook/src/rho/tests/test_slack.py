"""Routing a workset's Slack: the poll, the hook, and posts that take a
thread. Which agent hears what is Rust's, and tested there; here the fake
`_rho` answers with routes a test sets."""

import asyncio
import time

from slack_sdk.rho import Events
from slack_sdk.web.async_client import AsyncWebClient

from . import fake_rho
from .support import WorksetCase, add_agent, claude, wait_for

from rho import _boot, slack


def message(ts, text, thread_ts=None):
    event = {"type": "message", "channel": "C1", "user": "UPERSON", "ts": ts, "text": text}
    if thread_ts is not None:
        event["thread_ts"] = thread_ts
    return event


class FakeHost:
    """The host's Slack server: `auth.test` and the batches `rho.events`
    returns, then nothing."""

    def __init__(self, test, batches):
        self.batches = list(batches)
        self.polls = 0
        originals = (Events.poll, AsyncWebClient.auth_test)

        async def poll(events):
            self.polls += 1
            if self.batches:
                return self.batches.pop(0)
            await asyncio.sleep(3600)

        async def auth_test(client, **kwargs):
            return {"ok": True, "user_id": "UBOT"}

        Events.poll, AsyncWebClient.auth_test = poll, auth_test

        def restore():
            Events.poll, AsyncWebClient.auth_test = originals

        test.addCleanup(restore)


class Routing(WorksetCase):
    def setUp(self):
        self.host = FakeHost(self, [
            [message("1.0", "<@UBOT> hi"), message("2.0", "nobody hears this")],
            [{"type": "rho_truncated"}, message("1.1", "a reply", thread_ts="1.0")],
        ])
        super().setUp()

    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")
        self.child = add_agent(state, claude(), cwd="/")
        state.slack_owner = self.main
        state.slack_routes = {"1.0": self.main, "1.1": self.child}

    def test_each_heard_event_reaches_its_agent_as_a_tool_input(self):
        wait_for(lambda: len(self.state.inputs) == 3, "three Slack inputs")
        self.assertEqual(self.state.inputs, [
            (self.main, "[Slack C1 thread 1.0, from <@UPERSON>]\n<@UBOT> hi", "tool", None),
            (self.main, slack.render({"type": "rho_truncated"}), "tool", None),
            (self.child, "[Slack C1 thread 1.0, from <@UPERSON>]\na reply", "tool", None),
        ])
        # Rust was asked about each message, with the bot's own user id, and
        # not about the lost events.
        self.assertEqual(
            [args for name, args in self.state.calls if name == "slack_route"],
            [("1.0", "UBOT"), ("2.0", "UBOT"), ("1.1", "UBOT")],
        )


class NotRouted(WorksetCase):
    def setUp(self):
        self.host = FakeHost(self, [[message("1.0", "<@UBOT> hi")]])
        super().setUp()

    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")

    def test_a_workset_without_an_owner_does_not_poll_until_it_gets_one(self):
        self.assertIsNone(_boot._slack)
        self.assertEqual(self.host.polls, 0)
        fake_rho.STATE.slack_routes = {"1.0": self.main}
        slack.set_owner(self.main)
        wait_for(lambda: self.state.inputs, "the mention")
        self.assertEqual(self.state.slack_owner, self.main)
        # Setting it again does not start a second poll.
        slack.set_owner(self.main)
        time.sleep(0.2)
        self.assertEqual(self.host.polls, 2)

    def test_an_owner_from_another_workset_is_refused(self):
        with self.assertRaises(KeyError):
            slack.set_owner("agent-elsewhere")
        self.assertIsNone(self.state.slack_owner)


class Posting(WorksetCase):
    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")

    def post(self, reply):
        AsyncWebClient.posted(reply)

    def test_a_post_takes_its_thread_for_the_notebook_agent(self):
        self.state.slack_owner = self.main
        self.state.current = self.main
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.post({"ok": True, "channel": "C1", "ts": "6.1", "message": {"ts": "6.1", "thread_ts": "6.0"}})
        self.assertEqual(self.state.slack_claims, [("C1", "5.0", self.main), ("C1", "6.0", self.main)])

    def test_posts_take_nothing_off_a_notebook_or_in_a_workset_that_does_not_route(self):
        self.state.current = self.main
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.state.slack_owner = self.main
        self.state.current = None
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.assertEqual(self.state.slack_claims, [])
