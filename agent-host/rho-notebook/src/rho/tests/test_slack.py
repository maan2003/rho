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
#: A reaction to a message no agent wrote.
REACTION = {"type": "reaction_added", "user": "UPERSON", "reaction": "eyes", "item": {"type": "message", "channel": "C1", "ts": "3.0"}}
#: A reaction to the post 1.2, which an agent wrote in thread 1.0.
REACTION_TO_POST = {"type": "reaction_added", "user": "UPERSON", "reaction": "+1", "item": {"type": "message", "channel": "C1", "ts": "1.2"}}


class FakeHost:
    """The host's Slack server: `auth.test`, then the batches `rho.events`
    returns, then nothing."""

    def __init__(self, test, batches):
        self.batches = list(batches)
        self.polls = 0
        self.log = []
        originals = (Events.poll, AsyncWebClient.auth_test)

        async def poll(events, timeout=20):
            self.log.append(("poll", timeout))
            if timeout == 0:
                # Without a cursor the host starts at its newest event.
                return []
            self.polls += 1
            while not self.batches:
                await asyncio.sleep(0.01)
            return self.batches.pop(0)

        async def auth_test(client, **kwargs):
            self.log.append(("auth",))
            return {"ok": True, "user_id": "UBOT", "bot_id": "BBOT"}

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
        slack._bot_id = None
        slack._routing = False
        self.host = FakeHost(self, self.batches)
        super().setUp()


ROUTING = """
from rho import slack
got = []
slack.route_messages(lambda agent, event: got.append((agent.id, event.get("ts") or event.get("item") or event["type"])))
"""


class Routing(SlackCase):
    init = ROUTING
    batches = [
        [message("1.0", "<@UBOT> hi"), message("2.0", "nobody hears this"), JOINED, SOMEONE_JOINED, REACTION],
        [{"type": "rho_truncated"}, message("1.1", "a reply", thread_ts="1.0"), REACTION_TO_POST, dict(REACTION_TO_POST, type="reaction_removed")],
    ]

    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")
        self.child = add_agent(state, claude(), cwd="/")
        state.slack_owner = self.main
        state.slack_routes = {"1.0": (self.main, "1.0"), "1.1": (self.child, "1.0"), "1.2": (self.child, "1.0")}

    def test_each_heard_message_and_each_workspace_event_reaches_its_agent(self):
        got = workset.namespace["got"]
        wait_for(lambda: len(got) == 6, "six routed events")
        # A reaction comes with the thread of the post it is on.
        post = {"type": "message", "channel": "C1", "ts": "1.2", "thread_ts": "1.0"}
        self.assertEqual(got, [
            (self.main, "1.0"),
            (self.main, "member_joined_channel"),
            (self.main, "rho_truncated"),
            (self.child, "1.1"),
            (self.child, post),
            (self.child, post),
        ])
        # Rust was asked about each message and reaction, with the bot's own
        # user id, and about nothing else.
        self.assertEqual(
            [args for name, args in self.state.calls if name == "slack_who_hears"],
            [("message", "1.0", "UBOT", "BBOT"), ("message", "2.0", "UBOT", "BBOT"), ("reaction_added", "3.0", "UBOT", "BBOT"),
             ("message", "1.1", "UBOT", "BBOT"), ("reaction_added", "1.2", "UBOT", "BBOT"),
             ("reaction_removed", "1.2", "UBOT", "BBOT")],
        )
        self.assertEqual(
            slack.render({**REACTION_TO_POST, "item": post}),
            "[Slack C1 thread 1.0, ts 1.2] <@UPERSON> reacted with :+1: to your post.",
        )
        self.assertEqual(
            slack.render({**REACTION_TO_POST, "type": "reaction_removed", "item": post}),
            "[Slack C1 thread 1.0, ts 1.2] <@UPERSON> removed :+1: from your post.",
        )

    def test_the_cursor_is_set_before_slack_says_who_the_bot_is(self):
        # A reply that comes while `auth.test` waits is after the cursor.
        wait_for(lambda: len(self.host.log) >= 3, "three host calls")
        self.assertEqual(self.host.log[:3], [("poll", 0), ("auth",), ("poll", 20)])

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


class Shutdown(SlackCase):
    batches = []

    def test_a_handler_of_a_stopped_notebook_is_dropped(self):
        # A notebook that shuts down stops its loop but does not close it.
        loop = asyncio.new_event_loop()
        thread = threading.Thread(target=loop.run_forever)
        thread.start()

        async def add():
            slack.add_event_handler(lambda event: None)

        asyncio.run_coroutine_threadsafe(add(), loop).result(5)
        loop.call_soon_threadsafe(loop.stop)
        thread.join(5)
        self.assertFalse(loop.is_closed())
        self.assertEqual(len(slack._handlers), 1)
        self.host.batches.append([message("2.0", "after the shutdown")])
        wait_for(lambda: not slack._handlers, "the handler dropped")
        tasks = asyncio.all_tasks(loop)
        for task in tasks:
            task.cancel()
        loop.run_until_complete(asyncio.gather(*tasks, return_exceptions=True))
        loop.close()


class Posting(SlackCase):
    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")

    def post(self, reply):
        AsyncWebClient.posted(reply)

    def test_a_post_is_the_notebook_agents_and_routes_its_thread_to_it(self):
        slack._routing = True
        self.state.current = self.main
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.post({"ok": True, "channel": "C1", "ts": "6.1", "message": {"ts": "6.1", "thread_ts": "6.0"}})
        self.assertEqual(self.state.slack_posts, [("C1", "5.0", "5.0", self.main), ("C1", "6.1", "6.0", self.main)])

    def test_posts_route_nothing_off_a_notebook_or_in_a_workset_that_does_not_route(self):
        self.state.current = self.main
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        slack._routing = True
        self.state.current = None
        self.post({"ok": True, "channel": "C1", "ts": "5.0", "message": {"ts": "5.0"}})
        self.assertEqual(self.state.slack_posts, [])


class Naming(SlackCase):
    """Agents name each other "eng-<id>"; the routes keep bare ids."""

    def seed(self, state):
        self.main = add_agent(state, claude(), cwd="/")
        state.elsewhere.add("zzelsewhere")

    def test_a_route_takes_an_agent_id_as_agents_write_it_also_a_short_one(self):
        slack.route(f"eng-{self.main}", "C1")
        slack.route(self.main, "C1", thread_ts="1.0")
        slack.route(f"eng-{self.main[:4]}", "C2")
        routed = [args for name, args in self.state.calls if name == "slack_route"]
        self.assertEqual(routed, [(self.main, "C1", None, None), (self.main, "C1", "1.0", None), (self.main, "C2", None, None)])

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
