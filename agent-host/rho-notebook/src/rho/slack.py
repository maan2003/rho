"""Who in this workset hears Slack: the workset's routes, which Rust keeps in
the workset's state directory across restarts ([ARCH-agent-host](../../../specs/ARCH-agent-host.md#slack)).

A workset routes Slack once it has an owner (`set_owner`). Then one poll of
the host's Slack events serves the whole workset, and each message someone
hears goes to one agent through `hooks.on_slack`. A message is heard when it
mentions the bot, is in a thread an agent owns, or is in a channel whose
mode delivers it. It goes to the thread's owner, else the channel's, else
the workset's. A mention gives its thread to the agent it goes to, and so
does a post an agent makes in a thread nobody owns. Direct messages, edits,
reactions and the bot's own posts are not delivered.
"""

import asyncio
import sys
import traceback

import _rho
from slack_sdk.web.async_base_client import AsyncBaseClient

from . import agents, hooks

#: What a channel delivers besides mentions and owned threads.
MODES = ("mentions", "top_level", "all")

#: Seconds before a failed poll is tried again.
RETRY_SECONDS = 5


def owner() -> str | None:
    """The agent that gets what no rule gives another; None while the
    workset does not route Slack."""
    return _rho.slack_owner()


def set_owner(agent: str) -> None:
    """Route the workset's Slack, with `agent` as its owner."""
    _rho.set_slack_owner(agents.Agent(agent).id)
    from . import _boot

    _boot.serve_slack_soon()


def delegate(channel: str, to: str, thread_ts: str | None = None) -> None:
    """Give one thread, or with no `thread_ts` the whole channel, to agent
    `to`. A thread's owner wins over its channel's."""
    _rho.slack_delegate(channel, thread_ts, agents.Agent(to).id)


def set_mode(channel: str, mode: str) -> None:
    """What `channel` delivers besides mentions and owned threads: nothing
    more ("mentions", where every channel starts), each new top-level
    message ("top_level"), or every message ("all")."""
    _rho.slack_set_mode(channel, mode)


def watch(channel: str, thread_ts: str) -> str:
    """Hear a thread without posting in it: the calling agent takes it
    unless another agent owns it. Returns the thread's owner."""
    agent = _rho.current_agent()
    if agent is None:
        raise RuntimeError("rho.slack.watch: only an agent's notebook watches a thread")
    return _rho.slack_claim(channel, thread_ts, agent)


def routes() -> dict:
    """Every route: `{"owner": agent, "channels": {channel: {"owner",
    "mode"}}, "threads": {(channel, thread_ts): agent}}`."""
    return _rho.slack_routes()


def render(event: dict) -> str:
    """An event as an agent reads it: where it is first, so the agent can
    answer there."""
    if event["type"] == "rho_truncated":
        return "[Slack] The agent host lost Slack events. Read your threads again with `conversations_replies`."
    thread = event.get("thread_ts") or event["ts"]
    return f"[Slack {event['channel']} thread {thread}, from <@{event.get('user') or event.get('bot_id')}>]\n{event.get('text', '')}"


def _posted(response: dict) -> None:
    """A notebook's `chat.postMessage` went through: its agent takes the
    thread unless another agent owns it."""
    agent = _rho.current_agent()
    if agent is None or _rho.slack_owner() is None:
        return
    thread = response.get("message", {}).get("thread_ts") or response["ts"]
    _rho.slack_claim(response["channel"], thread, agent)


AsyncBaseClient.posted = staticmethod(_posted)


async def serve() -> None:
    """Hand each heard event to its agent while the process runs. The host
    keeps events only from now on, so a restart says it lost some."""
    from slack_sdk.errors import SlackApiError
    from slack_sdk.rho import Events
    from slack_sdk.web.async_client import AsyncWebClient

    client = AsyncWebClient()
    events = Events(client, _Log())
    bot = None
    while True:
        try:
            if bot is None:
                bot = (await client.auth_test())["user_id"]
            for event in await events.poll():
                if event["type"] == "rho_truncated":
                    to = owner()
                else:
                    to = await asyncio.to_thread(_rho.slack_route, event, bot)
                if to is not None:
                    await _hand(to, event)
        except Exception as error:
            print("rho: routing Slack:", file=sys.stderr)
            if isinstance(error, SlackApiError):
                print(f"the host refused: {error.response['error']}", file=sys.stderr)
            else:
                traceback.print_exc()
            await asyncio.sleep(RETRY_SECONDS)


async def _hand(to: str, event: dict) -> None:
    try:
        result = hooks.on_slack(agents.Agent(to), event)
        if asyncio.iscoroutine(result):
            await result
    except Exception:
        print(f"rho: handing a Slack event to {to}:", file=sys.stderr)
        traceback.print_exc()


class _Log:
    """`Events` says on stderr that the host is unreachable."""

    def warning(self, text):
        print(f"rho: {text}", file=sys.stderr)
