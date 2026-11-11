"""The agent host's Slack event buffer, and downloads of files Slack hosts.

Not part of upstream slack_sdk. `Events` long-polls `rho.events`; the
workset's `rho.slack` polls once for all its handlers and routes.
"""

import asyncio
import logging
import os
from pathlib import Path
from typing import List, Optional

import httpx

from slack_sdk.errors import SlackApiError
from slack_sdk.web.async_client import AsyncWebClient


class Events:
    """The host's Slack events in order, one long poll of `rho.events` at a
    time, from the first poll on. `{"type": "rho_truncated"}` comes first in
    a batch when the host lost events before it."""

    def __init__(self, client: AsyncWebClient, logger: logging.Logger):
        self.client = client
        self.logger = logger
        self._cursor: Optional[str] = None
        self._unreachable = False

    async def poll(self, timeout: float = 20) -> List[dict]:
        """The next events, after at most about `timeout` seconds. Raises
        `SlackApiError` when the host refuses the poll. While the host is
        unreachable it says so once and returns nothing after 5 seconds."""
        try:
            reply = await self.client.api_call(
                "rho.events", http_verb="GET", params={"cursor": self._cursor, "timeout": timeout}
            )
        except httpx.HTTPError as e:
            # The host may be restarting; say so once, then keep trying.
            if not self._unreachable:
                self.logger.warning(f"The agent host's Slack server is unreachable: {e}")
                self._unreachable = True
            await asyncio.sleep(5)
            return []
        self._unreachable = False
        self._cursor = reply["cursor"]
        events = [{"type": "rho_truncated"}] if reply["truncated"] else []
        for envelope in reply["events"]:
            event = envelope.get("payload", {}).get("event")
            if event:
                events.append(event)
        return events


async def download(url: str, *, timeout: float = 300) -> bytes:
    """The bytes of a file Slack hosts, such as `url_private_download` from
    `files_info`. The host adds the bot token and fetches only
    `https://files.slack.com/` URLs. Raises `SlackApiError` when the host or
    Slack refuses."""
    socket = str(Path(os.environ["RHO_SOCKET_PATH"]).with_name("slack.sock"))
    async with httpx.AsyncClient(transport=httpx.AsyncHTTPTransport(uds=socket), timeout=timeout) as client:
        resp = await client.get("http://slack/download", params={"url": url})
    if resp.status_code != 200:
        try:
            error = resp.json()["error"]
        except (ValueError, KeyError, TypeError):
            error = f"HTTP {resp.status_code}"
        raise SlackApiError(f"Downloading {url} failed", {"ok": False, "error": error})
    return resp.content
