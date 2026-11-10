"""A stand-in for the native `_rho` module, keeping its state in memory.
It follows `rho/_rho.pyi`; tests read and seed `STATE`."""

import asyncio
import copy
import itertools
import os
import re
import threading


class State:
    def __init__(self):
        self.kind = "isolated"
        self.props = {}
        self.permissions = None
        self.agents = {}
        self.elsewhere = set()  # agents of the host's other worksets
        self.events = 0  # the chat log's newest seq
        self.streams = {}  # name -> id
        self.directs = {}  # (a, b), a < b -> topic
        self.topics = {}  # id -> TopicRecord
        self.messages = []  # MessageRecord, in order
        self.inputs = []  # (agent, text, role, message)
        self.calls = []  # (name, args)
        self.endpoint = "ab" * 32  # the host's own; None for a host without iroh
        self.hosts = set()  # the endpoints the user approved
        self.contacts = {}  # (agent, address) -> name
        self.sent = []  # (agent, address, text) that the address's host took
        self.refusing = set()  # addresses whose host refuses
        self.root = "/workset"  # where the workset's files are
        self.services = {}  # (agent, address, service) -> socket path the agent opened
        self.forwards = {}  # socket path -> (agent, address, service) it leads to
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


def _post(sender, topic, text, mentions):
    """With the lock held. Who receives a message is fixed when it is
    posted, as the host's inbox index fixes it."""
    if topic not in STATE.topics:
        raise ValueError(f"no topic {topic}")
    if not text.strip():
        raise ValueError("a message needs text")
    head = STATE.topics[topic]
    if head["stream"] is None and sender not in head["agents"]:
        raise ValueError("only its two agents post in a direct conversation")
    for address, kind in mentions:
        assert kind in ("ask", "result", "fyi"), kind
        assert address == "user" or address in STATE.agents, address
    to = [agent for agent in head["agents"] + [address for address, _ in mentions] if agent not in ("user", sender)]
    id = _event()
    STATE.messages.append({"id": id, "at": 0, "topic": topic, "sender": sender, "mentions": list(mentions), "text": text, "_to": set(to)})
    return id


def _start(sender, stream, name, text, mentions):
    stream, name = stream.strip(), name.strip()
    if not stream or not name:
        raise ValueError("a topic needs a stream and a name")
    if stream not in STATE.streams:
        STATE.streams[stream] = _event()
    topic = _event()
    STATE.topics[topic] = {"id": topic, "name": name, "stream": stream, "resolved": False, "agents": []}
    return topic, _post(sender, topic, text, mentions)


def _public(message):
    return {key: value for key, value in copy.deepcopy(message).items() if not key.startswith("_")}


def post(sender, topic, text, *, mentions=None):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        return _post(sender, topic, text, mentions or [])


def start_topic(sender, stream, name, text, *, mentions=None):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        return _start(sender, stream, name, text, mentions or [])


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
            {
                "address": address,
                "name": name,
                "services": {
                    service: path for (a, to, service), path in STATE.services.items() if (a, to) == (holder, address)
                },
            }
            for (holder, address), name in STATE.contacts.items()
            if holder == agent
        ]


def _service(agent, address, service):
    assert agent in STATE.agents
    _endpoint(address)
    if (agent, address) not in STATE.contacts:
        raise RuntimeError(f"{address} is not an address you accepted")
    if not re.fullmatch(r"[a-z0-9._-]{1,64}", service):
        raise RuntimeError(f"a service name is 1 to 64 of a-z, 0-9, '.', '_' and '-', not {service!r}")


def external_expose(agent, address, service, path):
    with STATE.lock:
        _service(agent, address, service)
        if not path.startswith(STATE.root + "/"):
            raise ValueError(f"{path} is not in the workset ({STATE.root})")
        STATE.services[agent, address, service] = path


def external_unexpose(agent, address, service):
    with STATE.lock:
        _service(agent, address, service)
        if STATE.services.pop((agent, address, service), None) is None:
            raise RuntimeError(f"no service {service} is open to {address}")


def external_connect(agent, address, service, path):
    with STATE.lock:
        _service(agent, address, service)
        STATE.forwards[path] = (agent, address, service)


def direct(sender, other):
    with STATE.lock:
        assert sender in STATE.agents and sender != other
        if other not in STATE.agents and other not in STATE.elsewhere:
            raise ValueError("agent was deleted or does not exist")
        pair = tuple(sorted((sender, other)))
        if pair not in STATE.directs:
            topic = _event()
            STATE.topics[topic] = {"id": topic, "name": None, "stream": None, "resolved": False, "agents": list(pair)}
            STATE.directs[pair] = topic
        return STATE.directs[pair]


def join(topic, agent):
    with STATE.lock:
        head = STATE.topics[topic]
        if head["stream"] is None:
            raise ValueError("no one joins a direct conversation")
        if agent not in head["agents"]:
            head["agents"].append(agent)


def leave(topic, agent):
    with STATE.lock:
        head = STATE.topics[topic]
        if head["stream"] is None:
            raise ValueError("no one leaves a direct conversation")
        if agent in head["agents"]:
            head["agents"].remove(agent)


def change_topic(agent, topic, stream, name, resolved):
    with STATE.lock:
        if STATE.topics[topic]["stream"] is None:
            raise ValueError("a direct conversation is not renamed, moved or resolved")
        if stream not in STATE.streams:
            STATE.streams[stream] = _event()
        STATE.topics[topic].update(stream=stream, name=name, resolved=resolved)


def move_messages(agent, messages, to=None, *, stream=None, name=None):
    with STATE.lock:
        if to is None:
            if stream not in STATE.streams:
                STATE.streams[stream] = _event()
            to = _event()
            STATE.topics[to] = {"id": to, "name": name, "stream": stream, "resolved": False, "agents": []}
        for message in STATE.messages:
            if message["id"] in messages:
                message["topic"] = to
        return to


def topic(id):
    with STATE.lock:
        return copy.deepcopy(STATE.topics.get(id))


def message(id):
    with STATE.lock:
        return next((_public(m) for m in STATE.messages if m["id"] == id), None)


def read(topic, last):
    with STATE.lock:
        return [_public(m) for m in STATE.messages if m["topic"] == topic][-last:] if last else []


def post_user(topic, text, mentions=()):
    """What the host does with the GUI's Post: the user's mentions ask."""
    with STATE.lock:
        return _post("user", topic, text, [(agent, "ask") for agent in mentions])


def start_user_topic(stream, name, text, mentions=()):
    """What the host does with the GUI's StartTopic."""
    with STATE.lock:
        return _start("user", stream, name, text, [(agent, "ask") for agent in mentions])


async def inbox(agent, after):
    while True:
        with STATE.lock:
            found = [_public(m) for m in STATE.messages if m["id"] > after and agent in m["_to"]]
        if found:
            return found
        await asyncio.sleep(0.005)


def current_agent():
    return None
