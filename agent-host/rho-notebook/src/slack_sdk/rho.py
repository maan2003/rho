"""Thread subscriptions over the agent host's Slack event buffer.

Not part of upstream slack_sdk. An agent subscribes to the threads it
cares about. One long poll of `rho.events` serves them all and calls each
thread's callback with the raw Slack event; the callback decides what to
do, such as calling `notify()`. Subscriptions live in this process only.
"""

import asyncio
import inspect
import logging
from typing import Any, Callable, Dict, Optional, Tuple

from slack_sdk.web.async_client import AsyncWebClient

Callback = Callable[[dict], Any]


class ThreadSubscriptions:
    """`subscribe(channel, thread_ts, callback)` calls `callback(event)` for
    each new message, edit or deletion in that thread, and each reaction to
    its root message. Sync and async callbacks both work. When the
    host lost events, it calls `callback({"type": "rho_truncated"})`: read
    the thread again with `conversations_replies`."""

    def __init__(self, client: Optional[AsyncWebClient] = None, logger: Optional[logging.Logger] = None):
        self.client = client or AsyncWebClient()
        self.logger = logger or logging.getLogger(__name__)
        self.subscriptions: Dict[Tuple[str, str], Callback] = {}
        self._poller: Optional[asyncio.Task] = None
        self._cursor: Optional[str] = None

    def subscribe(self, channel: str, thread_ts: str, callback: Callback) -> None:
        self.subscriptions[(channel, thread_ts)] = callback
        if self._poller is None or self._poller.done():
            # From now: what happened before subscribing is in conversations_replies.
            self._cursor = None
            self._poller = asyncio.ensure_future(self._poll())

    def unsubscribe(self, channel: str, thread_ts: str) -> None:
        self.subscriptions.pop((channel, thread_ts), None)
        if not self.subscriptions and self._poller is not None:
            self._poller.cancel()
            self._poller = None

    async def _poll(self) -> None:
        while self.subscriptions:
            try:
                reply = await self.client.api_call(
                    "rho.events", http_verb="GET", params={"cursor": self._cursor, "timeout": 20}
                )
            except Exception as e:
                self.logger.warning(f"Failed to poll rho.events: {e}")
                await asyncio.sleep(5)
                continue
            self._cursor = reply["cursor"]
            if reply["truncated"]:
                for callback in list(self.subscriptions.values()):
                    await self._call(callback, {"type": "rho_truncated"})
            for envelope in reply["events"]:
                event = envelope.get("payload", {}).get("event")
                callback = event and self.subscriptions.get(_thread(event))
                if callback:
                    await self._call(callback, event)

    async def _call(self, callback: Callback, event: dict) -> None:
        try:
            result = callback(event)
            if inspect.isawaitable(result):
                await result
        except Exception:
            self.logger.exception("A thread subscription callback failed")


def _thread(event: dict) -> Tuple[Optional[str], Optional[str]]:
    """The (channel, thread root ts) an event belongs to."""
    # Edits and deletions carry the message one level down; reactions name
    # theirs as an item.
    item = event.get("item") or {}
    message = event.get("message") or event.get("previous_message") or event
    channel = event.get("channel") or item.get("channel")
    return channel, message.get("thread_ts") or message.get("ts") or item.get("ts")
