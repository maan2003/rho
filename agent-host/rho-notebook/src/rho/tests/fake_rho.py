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
        self.elsewhere = set()  # agents of the host's other worksets
        self.events = 0  # the chat log's newest seq
        self.streams = {}  # name -> id
        self.directs = {}  # (a, b), a < b -> topic
        self.endpoint = "ab" * 32  # the host's iroh endpoint id; None for a host without one
        self.peers = {}  # (agent, address) -> its open peer conversation
        self.topics = {}  # id -> TopicRecord
        self.messages = []  # MessageRecord, in order
        self.inputs = []  # (agent, text, role, message)
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
    if head["peer"] is not None:
        if not head["peer"]["open"]:
            raise ValueError(f"{head['peer']['name']} is no longer accepted")
        if sender != "peer" and sender not in head["agents"]:
            raise ValueError("only its agent and its peer post in a peer conversation")
    elif sender == "peer":
        raise ValueError("a peer posts only in its peer conversation")
    elif head["stream"] is None and sender not in head["agents"]:
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
    STATE.topics[topic] = {"id": topic, "name": name, "stream": stream, "resolved": False, "agents": [], "peer": None}
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


def direct(sender, other):
    with STATE.lock:
        assert sender in STATE.agents and sender != other
        if other not in STATE.agents and other not in STATE.elsewhere:
            raise ValueError("agent was deleted or does not exist")
        pair = tuple(sorted((sender, other)))
        if pair not in STATE.directs:
            topic = _event()
            STATE.topics[topic] = {"id": topic, "name": None, "stream": None, "resolved": False, "agents": list(pair), "peer": None}
            STATE.directs[pair] = topic
        return STATE.directs[pair]


def _address(address):
    endpoint, slash, local = address.partition("/")
    if not slash or len(endpoint) != 64 or not local:
        raise RuntimeError(f"an address is <endpoint id>/<local id>, not {address!r}")


def peer_address(agent):
    with STATE.lock:
        assert agent in STATE.agents, agent
        if STATE.endpoint is None:
            raise RuntimeError("this host has no iroh endpoint, so its agents have no address: start rho-agent-host with --iroh")
        return f"{STATE.endpoint}/{agent}"


def peer_accept(agent, address, name):
    with STATE.lock:
        assert agent in STATE.agents, agent
        _address(address)
        name = name.strip()
        if not name:
            raise RuntimeError("an accepted address needs a name")
        if (agent, address) in STATE.peers:
            topic = STATE.peers[agent, address]
            held = STATE.topics[topic]["peer"]["name"]
            if held != name:
                raise RuntimeError(f"{address} is already accepted as {held!r}; remove it to name it again")
            return topic
        topic = _event()
        peer = {"address": address, "name": name, "open": True}
        STATE.topics[topic] = {"id": topic, "name": None, "stream": None, "resolved": False, "agents": [agent], "peer": peer}
        STATE.peers[agent, address] = topic
        return topic


def peer_topic(agent, address):
    with STATE.lock:
        _address(address)
        if (agent, address) not in STATE.peers:
            raise RuntimeError(f"{address} is not accepted: accept it under a name first")
        return STATE.peers[agent, address]


def peers(agent):
    with STATE.lock:
        return [
            {"topic": topic, "address": address, "name": STATE.topics[topic]["peer"]["name"], "unsent": 0}
            for (owner, address), topic in STATE.peers.items()
            if owner == agent
        ]


def receive_peer(topic, text):
    """What the host does with a message the peer of `topic` sent."""
    with STATE.lock:
        return _post("peer", topic, text, [])


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
            STATE.topics[to] = {"id": to, "name": name, "stream": stream, "resolved": False, "agents": [], "peer": None}
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
