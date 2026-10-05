"""The request transport to the agent host's Slack server, for slack_sdk 3.45.0.

Upstream: https://github.com/slackapi/python-slack-sdk (dd615799e83ff20cd6b6acd2909ea19604679ef1).

Requests go to the host's Slack server over its Unix socket, and the server
adds the bot token. `async_base_client.py` sends them; the sync `BaseClient`
refuses to start.
"""

import os
from pathlib import Path
from typing import Any, BinaryIO, Dict, List, Tuple

import httpx

from .internal_utils import convert_bool_to_0_or_1, _build_unexpected_body_error_message
from .slack_response import SlackResponse  # noqa: F401 (client.py imports it from here)


def _socket() -> str:
    return str(Path(os.environ["RHO_SOCKET_PATH"]).with_name("slack.sock"))


def _request_kwargs(http_verb: str, api_url: str, req_args: dict) -> Tuple[List[BinaryIO], Dict[str, Any]]:
    """The files it opened, and the httpx request for slack_sdk request arguments."""
    opened: List[BinaryIO] = []
    files = {}
    for name, value in (req_args.get("files") or {}).items():
        if isinstance(value, str):
            value = open(value.encode("utf-8", "ignore"), "rb")
            opened.append(value)
        files[name] = value
    # httpx sets the content type, with the multipart boundary when there are files.
    headers = {k: v for k, v in (req_args.get("headers") or {}).items() if k.lower() not in ("content-type", "authorization")}
    return opened, dict(
        method=http_verb,
        url=api_url,
        headers=headers,
        params=convert_bool_to_0_or_1(req_args.get("params")),
        data=convert_bool_to_0_or_1(req_args.get("data")),
        files=files or None,
        json=req_args.get("json"),
    )


def _response(resp: httpx.Response) -> Dict[str, Any]:
    content_type = resp.headers.get("content-type", "")
    if content_type.startswith("application/gzip"):
        data: Any = resp.content
    else:
        try:
            data = resp.json()
        except ValueError:
            data = {"ok": False, "error": _build_unexpected_body_error_message(resp.text)}
    return {"status_code": resp.status_code, "headers": dict(resp.headers), "data": data}


class BaseClient:
    """The sync `WebClient` would block the notebook's event loop, which every
    task of the agent shares, so rho ships only the async client."""

    BASE_URL = "http://slack/api/"

    def __init__(self, *args, **kwargs):
        raise TypeError("rho supports only the async Slack client: use slack_sdk.web.async_client.AsyncWebClient")
