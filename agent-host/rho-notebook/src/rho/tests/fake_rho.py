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
        self.props = {}
        self.permissions = None
        self.agents = {}
        self.elsewhere = set()  # agents of the host's other worksets
        self.events = 0  # the chat log's newest seq
        self.messages = []  # MessageRecord, in order, with `_to`: who receives it
        self.members = {}  # (thread, agent) -> joined (True) or left
        self.names = {}  # thread -> name
        self.mail = []  # (sender, to, text) handed outside the chat
        self.inputs = []  # (agent, text, role, message)
        self.calls = []  # (name, args)
        self.endpoint = "ab" * 32  # the host's own; None for a host without iroh
        self.hosts = set()  # the endpoints the user approved
        self.contacts = {}  # (agent, address) -> name
        self.sent = []  # (agent, address, text) that the address's host took
        self.refusing = set()  # addresses whose host refuses
        self.contexts = {}  # cwd -> Context
        self.skill_dirs = {}  # directory -> [Skill]
        self.builtin = []  # [Skill]
        self.agents_files = {}  # path -> AgentsFile
        self.current = None  # the agent whose notebook is calling
        self.slack_owner = None  # the workspace route's agent
        self.slack_routes = {}  # Slack event ts, or a reacted post's ts -> (the agent it goes to, thread)
        self.slack_posts = []  # (channel, ts, thread_ts, agent)
        self.slack_table = {"workspace": None, "channels": {}, "threads": {}}  # what slack_routes returns
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


def workset_props():
    return dict(STATE.props)


def set_workset_props(props):
    STATE.props = dict(props)


def set_permissions(mode, environment=None):
    STATE.permissions = mode


def workset_permissions():
    return STATE.permissions


def clone(url, dest, rev=""):
    STATE.calls.append(("clone", (url, dest, rev)))
    os.makedirs(dest)


def repo_context(cwd):
    return copy.deepcopy(STATE.contexts.get(cwd, {"agents_files": [], "skills": []}))


def skills_in(directory):
    return copy.deepcopy(STATE.skill_dirs.get(directory, []))


def builtin_skills():
    return copy.deepcopy(STATE.builtin)


def agents_file(path):
    return copy.deepcopy(STATE.agents_files.get(path))


def models():
    return []


def create_agent(spec):
    with STATE.lock:
        id = f"agent{next(STATE.ids)}"
        STATE.agents[id] = copy.deepcopy(spec)
    return id


def runs(agent):
    with STATE.lock:
        return not (STATE.permissions == "auto" and STATE.agents[agent]["runtime"]["kind"] == "native")


def agents():
    with STATE.lock:
        return copy.deepcopy(STATE.agents)


def set_spec(agent, spec):
    with STATE.lock:
        assert agent in STATE.agents, agent
        STATE.agents[agent] = copy.deepcopy(spec)


def input(agent, text, *, role, message=None):
    assert role in ("user", "tool"), role
    with STATE.lock:
        assert agent in STATE.agents, agent
        STATE.inputs.append((agent, text, role, message))


def delivered(agent):
    with STATE.lock:
        return max((message for a, _, _, message in STATE.inputs if a == agent and message is not None), default=0)


def cancel(agent):
    STATE.calls.append(("cancel", (agent,)))


def compact(agent):
    STATE.calls.append(("compact", (agent,)))


def rewind(agent, turns):
    STATE.calls.append(("rewind", (agent, turns)))


def new_cache_key(agent):
    STATE.calls.append(("new_cache_key", (agent,)))


def _event():
    STATE.events += 1
    return STATE.events


def _message(id):
    return next((m for m in STATE.messages if m["id"] == id), None)


def _members_under(parent):
    """Whom a message under `parent` reaches by membership: for each agent
    with a record at `parent` or above, the nearest decides."""
    decided = {}
    thread = parent
    while thread is not None:
        for (at, agent), joined in STATE.members.items():
            if at == thread and agent not in decided:
                decided[agent] = joined
        thread = _message(thread)["parent"]
    return [agent for agent, joined in decided.items() if joined]


def _post(sender, parent, text, mentions):
    """With the lock held. Who receives a message is fixed when it is
    posted, as the host's inbox index fixes it."""
    if parent is not None and _message(parent) is None:
        raise ValueError(f"no message {parent}")
    if not text.strip():
        raise ValueError("a message needs text")
    for address, kind in mentions:
        assert kind in ("ask", "result", "fyi"), kind
        assert address == "user" or address in STATE.agents or address in STATE.elsewhere, address
    to = [agent for agent in _members_under(parent) + [address for address, _ in mentions] if agent not in ("user", sender)]
    id = _event()
    STATE.messages.append({"id": id, "at": 0, "parent": parent, "sender": sender, "mentions": list(mentions), "text": text, "_to": set(to)})
    return id


def _public(message):
    return {key: value for key, value in copy.deepcopy(message).items() if not key.startswith("_")}


def post(sender, parent, text, *, mentions=None):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        return _post(sender, parent, text, mentions or [])


def mail(sender, to, text):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        if to not in STATE.agents and to not in STATE.elsewhere:
            raise ValueError("agent was deleted or does not exist")
        if sender == to:
            raise ValueError("an agent does not mail itself")
        if not text.strip():
            raise ValueError("a message needs text")
        STATE.mail.append((sender, to, text))
        if to in STATE.agents:
            STATE.inputs.append((to, text, "tool", None))


def resolve_agent(prefix):
    with STATE.lock:
        found = [id for id in [*STATE.agents, *STATE.elsewhere] if id.startswith(prefix)] if prefix else []
    if len(found) != 1:
        raise RuntimeError(f"no agent {prefix} on this host" if not found else f"{prefix} names {len(found)} agents; give more of its id")
    return found[0]


def external_address(agent):
    with STATE.lock:
        assert agent in STATE.agents
        if STATE.endpoint is None:
            raise RuntimeError("this host has no iroh endpoint, so its agents have no address")
        return f"{STATE.endpoint}/{agent}"


def _endpoint(address):
    endpoint, slash, local = address.partition("/")
    if not slash or len(endpoint) != 64 or not local:
        raise RuntimeError(f"an address is <endpoint id>/<local id>, not {address!r}")
    return endpoint


def external_accept(agent, address, name):
    with STATE.lock:
        assert agent in STATE.agents
        endpoint = _endpoint(address)
        if endpoint not in STATE.hosts:
            raise RuntimeError(f"the user has not approved the host {endpoint}")
        held = STATE.contacts.setdefault((agent, address), name)
        if held != name:
            raise RuntimeError(f"{address} is already accepted as {held!r}")


async def external_send(agent, address, text):
    await asyncio.sleep(0)
    with STATE.lock:
        assert agent in STATE.agents
        _endpoint(address)
        if (agent, address) not in STATE.contacts:
            raise RuntimeError(f"{address} is not an address you accepted")
        name = STATE.contacts[agent, address]
        if address in STATE.refusing:
            raise RuntimeError(f"the host of {name} ({address}) refused the message")
        STATE.sent.append((agent, address, text))
        return name


def external_contacts(agent):
    with STATE.lock:
        return [
            {"address": address, "name": name}
            for (holder, address), name in STATE.contacts.items()
            if holder == agent
        ]


def join(thread, agent):
    with STATE.lock:
        if _message(thread) is None:
            raise ValueError(f"no message {thread}")
        STATE.members[thread, agent] = True


def leave(thread, agent):
    with STATE.lock:
        if _message(thread) is None:
            raise ValueError(f"no message {thread}")
        if agent in _members_under(thread):
            STATE.members[thread, agent] = False


def _above(message, thread):
    """Whether `thread` is `message` or above it."""
    while thread is not None:
        if thread == message:
            return True
        thread = _message(thread)["parent"]
    return False


def reparent(agent, message, parent):
    with STATE.lock:
        record = _message(message)
        if record is None or (parent is not None and _message(parent) is None):
            raise ValueError("no such message")
        if parent is not None and _above(message, parent):
            raise ValueError("a message does not go under its own thread")
        record["parent"] = parent


def name(agent, thread, name):
    with STATE.lock:
        if _message(thread) is None:
            raise ValueError(f"no message {thread}")
        name = " ".join(name.split())
        if name:
            STATE.names[thread] = name
        else:
            STATE.names.pop(thread, None)


def _name(id):
    return STATE.names.get(id) or _message(id)["text"].strip().splitlines()[0]


def thread(id):
    with STATE.lock:
        if _message(id) is None:
            return None
        path, cursor = [], id
        while cursor is not None:
            path.append((cursor, _name(cursor)))
            cursor = _message(cursor)["parent"]
        return {"id": id, "name": _name(id), "path": path[::-1], "agents": _members_under(id)}


def message(id):
    with STATE.lock:
        return next((_public(m) for m in STATE.messages if m["id"] == id), None)


def read(thread, last):
    with STATE.lock:
        if thread is not None and _message(thread) is None:
            raise ValueError(f"no message {thread}")
        return [_public(m) for m in STATE.messages if m["parent"] == thread][-last:] if last else []


def post_user(parent, text, mentions=()):
    """What the host does with the GUI's Post: the user's mentions ask."""
    with STATE.lock:
        return _post("user", parent, text, [(agent, "ask") for agent in mentions])


async def inbox(agent, after):
    while True:
        with STATE.lock:
            found = [_public(m) for m in STATE.messages if m["id"] > after and agent in m["_to"]]
        if found:
            return found
        await asyncio.sleep(0.005)


def current_agent():
    return STATE.current



def slack_who_hears(event, bot_user, bot_id):
    ts = event["ts"] if event["type"] == "message" else event["item"]["ts"]
    STATE.calls.append(("slack_who_hears", (event["type"], ts, bot_user, bot_id)))
    return STATE.slack_routes.get(ts)


def slack_owner(channel=None, thread_ts=None):
    return STATE.slack_owner


def slack_routes():
    return copy.deepcopy(STATE.slack_table)


def slack_posted(channel, ts, thread_ts, agent):
    STATE.slack_posts.append((channel, ts, thread_ts, agent))
    return agent


def slack_route(agent, channel=None, thread_ts=None, mode=None):
    STATE.calls.append(("slack_route", (agent, channel, thread_ts, mode)))
