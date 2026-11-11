"""The workset's Slack: one poll, its handlers, the routing loop, and posts
that route a thread. Which agent hears what is Rust's, and tested there;
here the fake `_rho` answers with routes a test sets."""

import asyncio
import threading
import time

from slack_sdk.rho import Events
from slack_sdk.web.async_client import AsyncWebClient

from .support import WorksetCase, add_agent, claude, wait_for

from rho import slack, workset


def message(ts, text, thread_ts=None, user="UPERSON"):
    event = {"type": "message", "channel": "C1", "user": user, "ts": ts, "text": text}
    if thread_ts is not None:
        event["thread_ts"] = thread_ts
    return event


JOINED = {"type": "member_joined_channel", "channel": "C2", "user": "UBOT"}
SOMEONE_JOINED = {"type": "member_joined_channel", "channel": "C2", "user": "UPERSON"}
REACTION = {"type": "reaction_added", "user": "UPERSON", "item": {"channel": "C1", "ts": "1.0"}}


class FakeHost:
    """The host's Slack server: `auth.test`, then the batches `rho.events`
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


class SlackCase(WorksetCase):
    batches = []

    def setUp(self):
        slack._handlers.clear()
        slack._poll = None
        slack._bot = None
        slack._routing = False
        self.host = FakeHost(self, self.batches)
        super().setUp()


ROUTING = """
from rho import slack
got = []
slack.route_messages(lambda agent, event: got.append((agent.id, event.get("ts") or event["type"])))
"""


class Routing(SlackCase):
    init = ROUTING
    batches = [
        [message("1.0", "<@UBOT> hi"), message("2.0", "nobody hears this"), JOINED, SOMEONE_JOINED, REACTION],
        [{"type": "rho_truncated"}, message("1.1", "a reply", thread_ts="1.0")],
    ]

    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")
        self.child = add_agent(state, claude(), cwd="/")
        state.slack_owner = self.main
        state.slack_routes = {"1.0": self.main, "1.1": self.child}

    def test_each_heard_message_and_each_workspace_event_reaches_its_agent(self):
        got = workset.namespace["got"]
        wait_for(lambda: len(got) == 4, "four routed events")
        self.assertEqual(got, [
            (self.main, "1.0"),
            (self.main, "member_joined_channel"),
            (self.main, "rho_truncated"),
            (self.child, "1.1"),
        ])
        # Rust was asked about each message, with the bot's own user id, and
        # about nothing else.
        self.assertEqual(
            [args for name, args in self.state.calls if name == "slack_who_hears"],
            [("1.0", "UBOT"), ("2.0", "UBOT"), ("1.1", "UBOT")],
        )

    def test_a_second_router_is_refused(self):
        with self.assertRaises(RuntimeError):
            slack.route_messages(lambda agent, event: None)


class Handlers(SlackCase):
    batches = [[REACTION, message("1.0", "hi")], [message("1.1", "after the removal")]]

    def test_a_handler_on_its_own_loop_sees_every_event_and_stops_when_removed(self):
        seen, removed = [], threading.Event()
        loop = asyncio.new_event_loop()

        async def notebook():
            remove = None

            async def handler(event):
                seen.append(event["type"])
                if len(seen) == 2:
                    remove()
                    removed.set()

            remove = slack.add_event_handler(handler)
            assert threading.get_ident() == thread.ident
            await asyncio.sleep(1)

        thread = threading.Thread(target=loop.run_until_complete, args=(notebook(),))
        thread.start()
        wait_for(removed.is_set, "the handler removed itself")
        wait_for(lambda: self.host.polls == 3, "the third poll")
        thread.join(5)
        loop.close()
        self.assertEqual(seen, ["reaction_added", "message"])

    def test_nothing_polls_without_a_handler(self):
        time.sleep(0.1)
        self.assertEqual(self.host.polls, 0)
        self.assertIsNone(slack._poll)


class Posting(SlackCase):
    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")

    def post(self, reply):
        AsyncWebClient.posted(reply)

    def test_a_post_routes_its_thread_to_the_notebook_agent(self):
        slack._routing = True
        self.state.current = self.main
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.post({"ok": True, "channel": "C1", "ts": "6.1", "message": {"ts": "6.1", "thread_ts": "6.0"}})
        self.assertEqual(self.state.slack_claims, [("C1", "5.0", self.main), ("C1", "6.0", self.main)])

    def test_posts_route_nothing_off_a_notebook_or_in_a_workset_that_does_not_route(self):
        self.state.current = self.main
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        slack._routing = True
        self.state.current = None
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.assertEqual(self.state.slack_claims, [])



class Naming(SlackCase):
    """Agents name each other "eng-<id>"; the routes keep bare ids."""

    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")
        state.elsewhere.add("zzelsewhere")

    def test_a_route_takes_an_agent_id_as_agents_write_it(self):
        slack.route(f"eng-{self.main}", "C1")
        slack.route(self.main, "C1", thread_ts="1.0")
        routed = [args for name, args in self.state.calls if name == "slack_route"]
        self.assertEqual(routed, [(self.main, "C1", None, None), (self.main, "C1", "1.0", None)])

    def test_a_route_names_an_agent_of_the_workset(self):
        with self.assertRaises(KeyError):
            slack.route("eng-zzelsewhere")
        with self.assertRaises(ValueError):
            slack.route("eng-nobody")
        with self.assertRaises(ValueError):
            slack.route(f"adv-{self.main}")

    def test_routes_name_agents_as_agents_write_them(self):
        self.assertIsNone(slack.owner("C1"))
        self.state.slack_owner = self.main
        self.assertEqual(slack.owner("C1"), f"eng-{self.main}")
        self.state.slack_table = {
            "workspace": {"agent": self.main, "mode": "mentions"},
            "channels": {"C1": {"agent": self.main, "mode": "all"}},
            "threads": {("C1", "1.0"): self.main},
        }
        label = f"eng-{self.main}"
        self.assertEqual(
            slack.routes(),
            {
                "workspace": {"agent": label, "mode": "mentions"},
                "channels": {"C1": {"agent": label, "mode": "all"}},
                "threads": {("C1", "1.0"): label},
            },
        )
