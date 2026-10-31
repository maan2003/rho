"""A stand-in for the native `_rho` module, keeping its state in memory.
It follows `rho/_rho.pyi`; tests read and seed `STATE`."""

import asyncio
import copy
import itertools
import os
import threading


class State:
    def __init__(self):
        self.kind = "isolated"
        self.agents = {}
        self.mails = []
        self.inputs = []  # (agent, text, wake, mail)
        self.calls = []  # (name, args)
        self.contexts = {}  # cwd -> Context
        self.ids = itertools.count(1)
        self.lock = threading.Lock()


STATE = State()


def reset():
    global STATE
    STATE = State()


def workset_id():
    return "wfake"


def workset_kind():
    return STATE.kind


def clone(url, dest, rev=""):
    STATE.calls.append(("clone", (url, dest, rev)))
    os.makedirs(dest)


def context(cwd):
    return copy.deepcopy(STATE.contexts.get(cwd, {"agents_files": [], "skills": []}))


def models():
    return []


def create_agent(spec):
    with STATE.lock:
        id = f"agent{next(STATE.ids)}"
        STATE.agents[id] = copy.deepcopy(spec)
    return id


def agents():
    with STATE.lock:
        return copy.deepcopy(STATE.agents)


def set_spec(agent, spec):
    with STATE.lock:
        assert agent in STATE.agents, agent
        STATE.agents[agent] = copy.deepcopy(spec)


def input(agent, text, *, wake, mail=None):
    assert wake in ("person", "agent", "quiet"), wake
    with STATE.lock:
        assert agent in STATE.agents, agent
        STATE.inputs.append((agent, text, wake, mail))


def delivered(agent):
    with STATE.lock:
        return max((mail for a, _, _, mail in STATE.inputs if a == agent and mail is not None), default=0)


def cancel(agent):
    STATE.calls.append(("cancel", (agent,)))


def compact(agent):
    STATE.calls.append(("compact", (agent,)))


def rewind(agent, turns):
    STATE.calls.append(("rewind", (agent, turns)))


def new_cache_key(agent):
    STATE.calls.append(("new_cache_key", (agent,)))


def send_mail(sender, to, text, *, kind=None, re=None):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        if to == "user" and kind is None:
            raise ValueError("a mail to the user needs a kind")
        id = len(STATE.mails) + 1
        STATE.mails.append({"id": id, "at": 0, "sender": sender, "to": to, "re": re, "kind": kind, "text": text})
        return id


def post_user_mail(to, text):
    """What the host does with the GUI's PostMail."""
    with STATE.lock:
        id = len(STATE.mails) + 1
        STATE.mails.append({"id": id, "at": 0, "sender": "user", "to": to, "re": None, "kind": None, "text": text})
        return id


async def inbox(address, after):
    while True:
        with STATE.lock:
            found = [dict(m) for m in STATE.mails if m["to"] == address and m["id"] > after]
        if found:
            return found
        await asyncio.sleep(0.005)


def current_agent():
    return None
