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
        self.events = 1  # the chat log's newest seq: 1 is the default stream
        self.topics = {}  # id -> TopicRecord
        self.messages = []  # MessageRecord, in order
        self.inputs = []  # (agent, text, wake, message)
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


def input(agent, text, *, wake, message=None):
    assert wake in ("person", "agent", "quiet"), wake
    with STATE.lock:
        assert agent in STATE.agents, agent
        STATE.inputs.append((agent, text, wake, message))


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


def _post(sender, topic, text, mentions):
    """With the lock held."""
    if topic not in STATE.topics:
        raise ValueError(f"no topic {topic}")
    if not text.strip():
        raise ValueError("a message needs text")
    for address, kind in mentions:
        assert kind in ("ask", "result", "fyi"), kind
        assert address == "user" or address in STATE.agents, address
    agents = STATE.topics[topic]["agents"]
    if sender != "user" and sender not in agents:
        agents.append(sender)
    STATE.events += 1
    STATE.messages.append({"id": STATE.events, "at": 0, "topic": topic, "sender": sender, "mentions": list(mentions), "text": text})
    return STATE.events


def _start(sender, stream, name, text, agents, mentions):
    assert stream == "rho", stream
    if not name.strip():
        raise ValueError("a topic needs a name")
    STATE.events += 1
    topic = STATE.events
    STATE.topics[topic] = {"id": topic, "name": name, "stream": stream, "agents": list(dict.fromkeys(agents))}
    _post(sender, topic, text, mentions)
    return topic


def post(sender, topic, text, *, mentions=None):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        return _post(sender, topic, text, mentions or [])


def start_topic(sender, stream, name, text, *, agents=[], mentions=None):
    with STATE.lock:
        if sender not in STATE.agents:
            raise ValueError(f"{sender} is not an agent of this workset")
        return _start(sender, stream, name, text, agents, mentions or [])


def join(topic, agent):
    with STATE.lock:
        agents = STATE.topics[topic]["agents"]
        if agent not in agents:
            agents.append(agent)


def topic(id):
    with STATE.lock:
        found = STATE.topics.get(id)
        return copy.deepcopy(found)


def post_user(topic, text):
    """What the host does with the GUI's Post."""
    with STATE.lock:
        return _post("user", topic, text, [])


def start_user_topic(name, text, agents):
    """What the host does with the GUI's StartTopic."""
    with STATE.lock:
        return _start("user", "rho", name, text, agents, [])


def _receives(agent, message):
    if message["sender"] == agent:
        return False
    mentioned = any(address == agent for address, _ in message["mentions"])
    return mentioned or agent in STATE.topics[message["topic"]]["agents"]


async def inbox(agent, after):
    while True:
        with STATE.lock:
            found = [copy.deepcopy(m) for m in STATE.messages if m["id"] > after and _receives(agent, m)]
        if found:
            return found
        await asyncio.sleep(0.005)


def current_agent():
    return None
