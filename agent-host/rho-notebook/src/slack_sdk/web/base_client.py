"""slack_sdk's sync `BaseClient`, refused in rho.

The sync `WebClient` would block the notebook's event loop, which every
task of the agent shares, so rho ships only the async client
(`async_base_client.py`).
"""

from .slack_response import SlackResponse  # noqa: F401 (client.py imports it from here)


class BaseClient:
    BASE_URL = "http://slack/api/"

    def __init__(self, *args, **kwargs):
        raise TypeError("rho supports only the async Slack client: use slack_sdk.web.async_client.AsyncWebClient")
