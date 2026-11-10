"""This process's workset."""

import os

import _rho

#: The workset's id, as encoded text.
id: str = _rho.workset_id()
#: "isolated" (a private /src) or "host" (a host directory).
kind: str = _rho.workset_kind()
#: The directory the workset is: /src for an isolated workset, and the
#: directory a host workset's process starts in.
root: str = "/src" if kind == "isolated" else os.getcwd()



def props() -> dict[str, str]:
    """What the workset is, as its policy named it: string props, empty
    until set. `profile` names the policy that init.py applies, and a
    workset with one never falls back to bypass."""
    return _rho.workset_props()


def set_props(props: dict[str, str]) -> None:
    """Name the workset's props, replacing any. Only before its first
    agent is created; they are then kept for good."""
    _rho.set_workset_props(props)


def set_permissions(mode: str, environment: list[str] | None = None) -> None:
    """What the agents' calls go through, for this run of the process:
    "bypass" (every call runs) or "auto" (Claude Code's auto mode reviews
    each call against `environment`, its `autoMode.environment`, or Claude
    Code's own when None; only Claude agents run). Once, before the first
    agent starts; init.py calls it at each start. Without it, the first
    agent fixes "bypass"."""
    _rho.set_permissions(mode, environment)


def permissions() -> str | None:
    """"bypass" or "auto", once fixed; None before."""
    return _rho.workset_permissions()


#: The globals init.py, the workset's creation code and GUI `Eval` run in.
namespace: dict = {"__name__": "__rho_workset__"}


def clone(url: str, dest: str, rev: str = "") -> None:
    """Clone `url` into `dest` from the host's mirror and detach at `rev`
    (empty: the remote's default branch). Blocking."""
    _rho.clone(url, dest, rev)
