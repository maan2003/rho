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

#: The globals init.py, the workset's creation code and GUI `Eval` run in.
namespace: dict = {"__name__": "__rho_workset__"}


def clone(url: str, dest: str, rev: str = "") -> None:
    """Clone `url` into `dest` from the host's mirror and detach at `rev`
    (empty: the remote's default branch). Blocking."""
    _rho.clone(url, dest, rev)
