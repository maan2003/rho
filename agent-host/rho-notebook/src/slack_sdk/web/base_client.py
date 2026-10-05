"""The slack_sdk 3.45.0 `BaseClient`, adapted for the agent host's Slack server.

Copyright (c) the slack_sdk contributors. MIT License.
Upstream: https://github.com/slackapi/python-slack-sdk (dd615799e83ff20cd6b6acd2909ea19604679ef1).

Requests go to the host's Slack server over its Unix socket, and the server
adds the bot token. The `WebClient` and `AsyncWebClient` methods above this
are upstream's.
"""

import json
import logging
import os
from pathlib import Path
from ssl import SSLContext
from typing import Any, BinaryIO, Dict, List, Optional, Tuple

import httpx

from .deprecation import show_deprecation_warning_if_any
from .file_upload_v2_result import FileUploadV2Result
from .internal_utils import (
    convert_bool_to_0_or_1,
    get_user_agent,
    _get_url,
    _build_req_args,
    _build_unexpected_body_error_message,
)
from .slack_response import SlackResponse


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
    BASE_URL = "http://slack/api/"

    def __init__(
        self,
        token: Optional[str] = None,
        base_url: str = BASE_URL,
        timeout: int = 30,
        ssl: Optional[SSLContext] = None,
        proxy: Optional[str] = None,
        headers: Optional[dict] = None,
        user_agent_prefix: Optional[str] = None,
        user_agent_suffix: Optional[str] = None,
        # for Org-Wide App installation
        team_id: Optional[str] = None,
        logger: Optional[logging.Logger] = None,
        retry_handlers: Optional[list] = None,
    ):
        # The host holds the token. One passed here is not sent.
        self.token = None
        if not base_url.endswith("/"):
            base_url += "/"
        self.base_url = base_url
        self.timeout = timeout
        self.ssl = ssl
        self.proxy = proxy
        self.headers = headers or {}
        self.headers["User-Agent"] = get_user_agent(user_agent_prefix, user_agent_suffix)
        self.default_params = {}
        if team_id is not None:
            self.default_params["team_id"] = team_id
        self._logger = logger if logger is not None else logging.getLogger(__name__)
        self.retry_handlers = retry_handlers or []

    @property
    def logger(self) -> logging.Logger:
        """The logger this client uses."""
        return self._logger

    def api_call(
        self,
        api_method: str,
        *,
        http_verb: str = "POST",
        files: Optional[dict] = None,
        data: Optional[dict] = None,
        params: Optional[dict] = None,
        json: Optional[dict] = None,
        headers: Optional[dict] = None,
        auth: Optional[dict] = None,
    ) -> SlackResponse:
        """Calls a Slack Web API method, e.g. `'chat.postMessage'`, through the host."""
        api_url = _get_url(self.base_url, api_method)
        headers = headers or {}
        headers.update(self.headers)
        req_args = _build_req_args(
            token=None,
            http_verb=http_verb,
            files=files,  # type: ignore[arg-type]
            data=data,  # type: ignore[arg-type]
            default_params=self.default_params,
            params=params,  # type: ignore[arg-type]
            json=json,  # type: ignore[arg-type]
            headers=headers,
            auth=None,  # type: ignore[arg-type]
            ssl=None,
            proxy=None,
        )
        show_deprecation_warning_if_any(api_method)
        response = self._request_for_pagination(api_url=api_url, req_args=req_args, http_verb=http_verb)
        return SlackResponse(client=self, http_verb=http_verb, api_url=api_url, req_args=req_args, **response).validate()

    def _request_for_pagination(self, api_url: str, req_args: dict, http_verb: str = "POST") -> Dict[str, Any]:
        """Sends one request. `SlackResponse` pagination calls this for each page."""
        opened, kwargs = _request_kwargs(http_verb, api_url, req_args)
        try:
            with httpx.Client(transport=httpx.HTTPTransport(uds=_socket()), timeout=self.timeout) as client:
                return _response(client.request(**kwargs))
        finally:
            for f in opened:
                f.close()

    def _upload_file(
        self,
        *,
        url: str,
        data: bytes,
        logger: logging.Logger,
        timeout: int,
        proxy: Optional[str],
        ssl: Optional[SSLContext],
    ) -> FileUploadV2Result:
        """Uploads to the URL that `files.getUploadURLExternal` issued. The URL
        itself authorizes the upload, so it goes straight to Slack."""
        resp = httpx.post(url, content=data, timeout=timeout)
        return FileUploadV2Result(status=resp.status_code, body=resp.text)
