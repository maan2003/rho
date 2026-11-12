"""Slack for the workset: one poll of the host's Slack events, the handlers
that see them, and the workset's routes, which Rust keeps across restarts
([ARCH-agent-host](../../../specs/ARCH-agent-host.md#slack)).

`add_event_handler` gives a handler every event, until its process or
notebook ends. `route_messages`, which a policy calls at each start, routes
new messages to agents: a route gives the workspace, a channel or one thread
to an agent, and the most specific route wins. A thread route hears its
whole thread; a workspace or channel route hears mentions of the bot and
what its mode adds. A mention, or a post from an agent's notebook, in a
thread with no route routes that thread to the agent. A reaction to a post
from an agent's notebook goes to that agent. Direct messages, edits, other
reactions and the bot's own posts are not routed; a handler of its own sees
them.
"""

import asyncio
import contextvars
import inspect
import sys
import traceback

import _rho
from slack_sdk.web.async_base_client import AsyncBaseClient

from . import agents
from .prompt import label
from .tools import resolve

#: What a workspace or channel route hears besides mentions of the bot:
#: nothing more, each new top-level message, or every message.
MODES = ("mentions", "top_level", "all")

#: Seconds before a failed poll is tried again.
RETRY_SECONDS = 5


class _Handler:
    """A handler, run on the loop it was added from, one event at a time."""

    def __init__(self, function, loop):
        self.function = function
        self.loop = loop
        self.queue = asyncio.Queue()
        self.removed = False
        self.task = loop.create_task(self._run())

    def give(self, event) -> bool:
        """Queue `event`; False once the handler's loop is gone. A notebook
        that shuts down stops its loop without closing it."""
        if self.task.done() or not self.loop.is_running():
            return False
        try:
            self.loop.call_soon_threadsafe(self.queue.put_nowait, event)
            return True
        except RuntimeError:
            return False

    async def _run(self):
        while True:
            event = await self.queue.get()
            if self.removed:
                return
            try:
                result = self.function(event)
                if inspect.isawaitable(result):
                    await result
            except Exception:
                print("rho: a Slack event handler failed:", file=sys.stderr)
                traceback.print_exc()


_handlers: list[_Handler] = []
_poll: asyncio.Task | None = None
_bot: str | None = None
_bot_id: str | None = None
_routing = False


def add_event_handler(handler):
    """Call `handler(event)`, sync or async, with each Slack event the host
    receives from now on: messages, edits, deletions, reactions, file
    changes and joins, as Slack sends them. It runs on the loop it was added
    from, a notebook's or the workset's, one event at a time.
    `{"type": "rho_truncated"}` means the host lost events, for example
    across its restart, and `{"type": "rho_error", "error": ...}` that it
    refuses the poll, such as without an app token; polling goes on. Lives
    until the process or the notebook ends.
    Returns a function that removes it."""
    entry = _Handler(handler, asyncio.get_running_loop())
    _handlers.append(entry)
    from . import _boot

    _boot._loop.call_soon_threadsafe(_start)

    def remove():
        entry.removed = True
        if entry in _handlers:
            _handlers.remove(entry)
        entry.give(None)

    return remove


def bot_user() -> str | None:
    """The bot's Slack user id, once the poll asked Slack."""
    return _bot


def route_messages(hand) -> None:
    """Route the workset's new Slack messages for this run of its process:
    each one that someone hears goes to `hand(agent, event)`, sync or async,
    on the workset loop. So does each reaction to an agent's post, with
    `thread_ts`, the post's thread, added to its `item`. The workspace
    route's agent also gets lost-event notices and the bot's joins to
    channels. Call once, at each start: a policy that does not call it
    routes nothing."""
    global _routing
    if _routing:
        raise RuntimeError("rho.slack.route_messages: the workset routes already")
    _routing = True

    async def on_event(event):
        kind = event["type"]
        if kind in ("message", "reaction_added"):
            heard = await asyncio.to_thread(_rho.slack_who_hears, event, _bot, _bot_id)
            if heard is None:
                return
            to, thread = heard
            if kind == "reaction_added":
                event = {**event, "item": {**event["item"], "thread_ts": thread}}
        elif kind == "rho_truncated" or (kind == "member_joined_channel" and event.get("user") == _bot):
            to = _rho.slack_owner()
        else:
            return
        if to is not None:
            result = hand(agents.Agent(to), event)
            if inspect.isawaitable(result):
                await result

    add_event_handler(on_event)


def route(agent_id: str, channel: str | None = None, thread_ts: str | None = None, mode: str | None = None) -> None:
    """Route the workspace, a channel, or with `thread_ts` one thread of the
    channel, to the agent `agent_id` names ("eng-<id>", "adv-<id>" or the
    bare id), replacing its route. A workspace or channel route hears
    mentions and what its `mode` adds ("mentions" without one); a thread
    route hears its whole thread and takes no mode."""
    _rho.slack_route(agents.Agent(resolve(agent_id)).id, channel, thread_ts, mode)


def unroute(channel: str | None = None, thread_ts: str | None = None) -> None:
    """Remove the route of the workspace, a channel or a thread."""
    _rho.slack_unroute(channel, thread_ts)


def owner(channel: str | None = None, thread_ts: str | None = None) -> str | None:
    """The agent id, as agents name it ("eng-<id>"), of the most specific
    route that covers the thread, the channel, or the workspace."""
    agent = _rho.slack_owner(channel, thread_ts)
    return None if agent is None else label(agent)


def routes() -> dict:
    """Every route, with agent ids as agents name them: `{"workspace":
    {"agent", "mode"} | None, "channels": {channel: {"agent", "mode"}},
    "threads": {(channel, thread_ts): agent}}`."""
    found = _rho.slack_routes()
    wide = lambda route: route and {**route, "agent": label(route["agent"])}
    return {
        "workspace": wide(found["workspace"]),
        "channels": {channel: wide(route) for channel, route in found["channels"].items()},
        "threads": {thread: label(agent) for thread, agent in found["threads"].items()},
    }


def render(event: dict) -> str:
    """An event as an agent reads it: where it is first, so the agent can
    answer there."""
    kind = event["type"]
    if kind == "rho_truncated":
        return "[Slack] The agent host lost Slack events. Read your threads again with `conversations_replies`."
    if kind == "member_joined_channel":
        return f"[Slack {event['channel']}] The bot was added to this channel."
    if kind == "reaction_added":
        item = event["item"]
        return f"[Slack {item['channel']} thread {item['thread_ts']}, ts {item['ts']}] <@{event['user']}> reacted :{event['reaction']}: to your post."
    thread = event.get("thread_ts") or event["ts"]
    return f"[Slack {event['channel']} thread {thread}, from <@{event.get('user') or event.get('bot_id')}>]\n{event.get('text', '')}"


def _posted(response: dict) -> None:
    """A notebook's `chat.postMessage` went through: in a workset that routes
    Slack, reactions to the post go to the notebook's agent, and its thread
    is routed to that agent unless it has a route."""
    agent = _rho.current_agent()
    if agent is None or not _routing:
        return
    thread = response.get("message", {}).get("thread_ts") or response["ts"]
    _rho.slack_posted(response["channel"], response["ts"], thread, agent)


AsyncBaseClient.posted = staticmethod(_posted)


def _start() -> None:
    """On the workset loop: poll, unless it does already."""
    global _poll
    if _poll is None:
        from . import _boot

        _poll = _boot.spawn(_serve())


async def _serve() -> None:
    from slack_sdk.errors import SlackApiError
    from slack_sdk.rho import Events
    from slack_sdk.web.async_client import AsyncWebClient

    global _bot, _bot_id
    client = AsyncWebClient()
    events = Events(client, _Log())
    # The first poll sets the cursor at once, so that what arrives while
    # Slack says who the bot is still reaches the handlers.
    first = None
    refused = False
    while True:
        try:
            if first is None:
                first = await events.poll(timeout=0)
            if _bot is None:
                me = await client.auth_test()
                _bot, _bot_id = me["user_id"], me["bot_id"]
            batch, first = first or await events.poll(), []
            for event in batch:
                for handler in list(_handlers):
                    if not handler.give(event) and handler in _handlers:
                        _handlers.remove(handler)
        except SlackApiError as error:
            # Said to the handlers once, until a poll succeeds.
            if not refused:
                refused = True
                for handler in list(_handlers):
                    handler.give({"type": "rho_error", "error": error.response["error"]})
            await asyncio.sleep(RETRY_SECONDS)
            continue
        except Exception:
            print("rho: polling Slack:", file=sys.stderr)
            traceback.print_exc()
            await asyncio.sleep(RETRY_SECONDS)
            continue
        refused = False


class _Log:
    """`Events` says on stderr that the host is unreachable."""

    def warning(self, text):
        print(f"rho: {text}", file=sys.stderr)
