"""Notion for agents: the tools of Notion's hosted MCP server.

Not an upstream package. Calls go to the agent host's Notion server, which
signs in to Notion as the user and forwards a fixed list of tools; see
`agent-host/notion-server`. Only async: a blocking call would hold the
notebook's event loop.

`await call("notion-fetch", id=page_id)` returns the tool's MCP result:
`{"content": [{"type": "text", "text": ...}], ...}`, and `text()` joins its
text. `await tools()` lists the tools agents may call, with their
descriptions and argument schemas. `watch()` reports new comments on a page.
"""

import asyncio
import html
import inspect
import json
import logging
import os
import re
from pathlib import Path
from typing import Any, Callable, Dict, List

import httpx

logger = logging.getLogger(__name__)


class NotionError(Exception):
    """The host refused the call, or the tool reported an error."""


async def tools() -> List[Dict[str, Any]]:
    return (await _request("GET", "/tools"))["tools"]


async def call(tool: str, **arguments: Any) -> Dict[str, Any]:
    result = await _request("POST", f"/tools/{tool}", json=arguments)
    if result.get("isError"):
        raise NotionError(text(result))
    return result


def text(result: Dict[str, Any]) -> str:
    """The text parts of a tool result, joined."""
    return "\n".join(part["text"] for part in result.get("content", []) if part.get("type") == "text")


async def _request(method: str, path: str, **kwargs: Any) -> Dict[str, Any]:
    socket = str(Path(os.environ["RHO_SOCKET_PATH"]).with_name("notion.sock"))
    async with httpx.AsyncClient(
        transport=httpx.AsyncHTTPTransport(uds=socket), base_url="http://notion", timeout=120
    ) as client:
        response = await client.request(method, path, **kwargs)
    body = response.json()
    if response.is_error:
        raise NotionError(body.get("error", response.text))
    return body


def watch(page_id: str, callback: Callable[[dict], Any], interval: float = 30) -> "asyncio.Task[None]":
    """Calls `callback(event)` for each comment added to the page from now
    on, inline ones included, checking every `interval` seconds. Cancel the
    returned task to stop. Sync and async callbacks both work.

    A comment event: `{"type": "comment", "page_id", "discussion_id",
    "comment_id", "user", "datetime", "context", "text"}`; reply to it with
    `notion-create-comment` and its `discussion_id`. Agents comment as the
    user, so your own comments arrive too: they start with 🤖.
    `{"type": "rho_error", "error": ...}` means the host refused the check;
    watching stops."""
    return asyncio.ensure_future(_watch(page_id, callback, interval))


async def _watch(page_id: str, callback: Callable[[dict], Any], interval: float) -> None:
    seen = None
    unreachable = False
    while True:
        try:
            result = await call("notion-get-comments", page_id=page_id, include_all_blocks=True)
        except NotionError as e:
            await _call(callback, {"type": "rho_error", "error": str(e)})
            return
        except httpx.HTTPError as e:
            # The host may be restarting; say so once, then keep trying.
            if not unreachable:
                logger.warning(f"The agent host's Notion server is unreachable: {e}")
                unreachable = True
            await asyncio.sleep(interval)
            continue
        unreachable = False
        comments = _comments(page_id, result)
        if seen is not None:
            for comment in comments:
                if comment["comment_id"] not in seen:
                    await _call(callback, comment)
        seen = {comment["comment_id"] for comment in comments}
        await asyncio.sleep(interval)


_DISCUSSION = re.compile(r'<discussion id="([^"]*)"([^>]*)>(.*?)</discussion>', re.S)
_COMMENT = re.compile(r'<comment id="([^"]*)"([^>]*)>(.*?)</comment>', re.S)


def _comments(page_id: str, result: Dict[str, Any]) -> List[dict]:
    """The comments in a `notion-get-comments` result, oldest first."""
    body = text(result)
    try:
        body = json.loads(body)["text"]
    except (ValueError, KeyError, TypeError):
        pass
    comments = []
    for discussion_id, attributes, discussion in _DISCUSSION.findall(body):
        context = _attribute(attributes, "text-context")
        for comment_id, comment_attributes, comment in _COMMENT.findall(discussion):
            comments.append({
                "type": "comment",
                "page_id": page_id,
                "discussion_id": discussion_id,
                "comment_id": comment_id,
                # user://<id>/<email>
                "user": _attribute(comment_attributes, "user-url"),
                "datetime": _attribute(comment_attributes, "datetime"),
                "context": context,
                "text": html.unescape(comment),
            })
    return comments


def _attribute(attributes: str, name: str) -> Any:
    match = re.search(rf'{name}="([^"]*)"', attributes)
    return html.unescape(match.group(1)) if match else None


async def _call(callback: Callable[[dict], Any], event: dict) -> None:
    try:
        result = callback(event)
        if inspect.isawaitable(result):
            await result
    except Exception:
        logger.exception("A Notion watch callback failed")
