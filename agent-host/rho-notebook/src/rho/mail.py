"""Mail between the user and agents, kept by the host ([ARCH-rho2]).
An address is "user" or an agent id."""

import _rho

USER = "user"


def send(sender: str, to: str, text: str, kind: str | None = None, re: int | None = None) -> int:
    """Append a mail; returns its id. `sender` is an agent of this workset.
    Mail to the user needs a `kind`: "ask", "result", "status" or "fyi"."""
    return _rho.send_mail(sender, to, text, kind=kind, re=re)


async def inbox(address: str, after: int) -> list[dict]:
    """Mail to `address` newer than `after`, waiting until there is some."""
    return await _rho.inbox(address, after)
