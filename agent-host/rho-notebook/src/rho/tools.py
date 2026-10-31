"""The tools the default policy puts in an agent's notebook: `chat`,
`agents` and `skills`. Rust adds `web`, `view_image`, `papercut`,
`end_turn` and `archive`.

A tool runs on the notebook thread of the agent that calls it. What it
reports, it prints, so the model sees it with its cell, and returns as an
awaitable string, so the model may `await` it or not.
"""

import os

import _rho

from . import agents as rho_agents
from . import chat as rho_chat
from . import skills as rho_skills
from .prompt import label

#: How many agents deep a spawn may be, counting the spawner.
MAX_SPAWN_DEPTH = 3


class Reply(str):
    """A tool's answer: a string that may also be awaited."""

    def __await__(self):
        return str(self)
        yield


def report(text: str) -> Reply:
    print(text)
    return Reply(text)


def native(model: str, effort: str) -> dict:
    return {"kind": "native", "model": model, "effort": effort, "fast": False}


def claude(model: str, effort: str) -> dict:
    return {"kind": "claude", "model": model, "effort": effort}


def engineer_runtime(spawner: dict) -> dict:
    """What an Engineer another one starts runs on: a Claude spawner's get
    Opus, a Terra spawner's Terra, and the rest Sol."""
    if spawner["kind"] == "claude":
        return claude("opus", "medium")
    if spawner["model"] == "terra":
        return native("terra", "xhigh")
    return native("sol", "high")


def advisor_runtime(asker: dict) -> dict:
    """What an Advisor runs on, from the runtime of the Engineer asking."""
    if asker["kind"] == "native" and asker["model"] == "terra":
        return native("sol", "xhigh")
    if asker["kind"] == "native" and asker["model"] == "astra":
        return claude("fable", "xhigh")
    return native("astra", "xhigh")


def resolve(handle: str) -> str:
    """The agent id a handle names: "eng-<id>", "adv-<id>" or the bare id.
    A prefix must match the agent's role."""
    handle = handle.strip()
    prefix, dash, raw = handle.partition("-")
    if not (dash and prefix in ("eng", "adv")):
        return handle
    known = _rho.agents()
    if raw in known and label(raw) != handle:
        raise ValueError(f"{handle} names {label(raw)}, whose role prefix differs")
    return raw


def _spawner_of(props: dict) -> str | None:
    return props.get("parent") or props.get("started_by")


class Chat:
    """Posting in the host's chat. Every message is in a topic; it reaches
    the topic's agents and the agents it mentions. The user is reached by a
    mention."""

    def __init__(self, agent: str):
        self._agent = agent

    def _mentions(self, mention, kind):
        if mention is None:
            if kind is not None:
                raise ValueError("a kind goes with a mention: mention=\"user\"")
            return []
        if kind not in rho_chat.KINDS:
            raise ValueError('a mention needs kind: "ask", "result" or "fyi"')
        handles = [mention] if isinstance(mention, str) else list(mention)
        mentions = []
        for handle in handles:
            if handle == rho_chat.USER:
                props = rho_agents.Agent(self._agent).props
                if props.get("preset") != "engineer":
                    parent = props.get("parent")
                    raise ValueError(f"you report to your parent agent {label(parent)}; post in your topic without a mention")
                mentions.append((rho_chat.USER, kind))
                continue
            agent = resolve(str(handle))
            if agent == self._agent:
                raise ValueError("cannot mention yourself")
            mentions.append((agent, kind))
        return mentions

    def post(self, topic, text, *, mention=None, kind=None):
        """Post `text` in `topic`, the id each message arrives with. To reach
        the user, mention "user" with a kind: "ask", "result" or "fyi"."""
        text = str(text)
        if not text.strip():
            raise ValueError("a message needs text")
        mentions = self._mentions(mention, kind)
        rho_chat.post(self._agent, int(topic), text, mentions)
        if any(address == rho_chat.USER for address, _ in mentions):
            return Reply("")
        return report(f"Posted in #{int(topic)}.")

    def start(self, name, text, *, agents=(), mention=None, kind=None):
        """Start a topic named `name` with `text` as its first message, for a
        new subject. `agents` receive its messages; returns its id."""
        text = str(text)
        if not text.strip():
            raise ValueError("a message needs text")
        mentions = self._mentions(mention, kind)
        members = [resolve(str(agent)) for agent in agents]
        topic = rho_chat.start(self._agent, str(name), text, agents=members, mentions=mentions)
        print(f"Started topic #{topic}.")
        return topic

    def __repr__(self):
        return "<chat>"


class Agents:
    """Starting and stopping other agents."""

    def __init__(self, agent: str):
        self._agent = agent

    def _me(self):
        return rho_agents.Agent(self._agent)

    def _check_depth(self):
        known = _rho.agents()
        depth, cursor = 0, self._agent
        while cursor is not None and cursor in known:
            depth += 1
            if depth > MAX_SPAWN_DEPTH:
                raise RuntimeError(f"spawn depth limit ({MAX_SPAWN_DEPTH}) reached")
            cursor = _spawner_of(known[cursor]["props"])

    def _spawn(self, runtime, props, name, prompt, mentions=()):
        """Create an agent and brief it in a new topic with this one. The
        topic is the agent's `topic` prop, where a failure is told."""
        self._check_depth()
        agent = rho_agents.create(runtime, props)
        topic = rho_chat.start(self._agent, name, prompt, agents=[self._agent, agent.id], mentions=mentions)
        agent.set_props({**props, "topic": str(topic)})
        return agent, topic

    def spawn_new_engineer(self, *, task_name, prompt, workdir=None):
        """Start an Engineer on `prompt`, in `workdir` or your directory."""
        if not str(prompt).strip():
            raise ValueError("prompt must not be empty")
        me = self._me()
        cwd = _workdir(workdir) or me.props["cwd"]
        props = {"name": task_name, "preset": "child", "parent": me.id, "cwd": cwd}
        child, topic = self._spawn(engineer_runtime(me.runtime), props, task_name, prompt)
        return report(
            f'Spawned agent {label(child.id)} for task "{task_name}" in topic #{topic}. It is '
            f"working now; its results will arrive in that topic. It works in {cwd}. Post in "
            f"#{topic} to follow up."
        )

    def spawn_new_advisor(self, msg):
        """Ask an independent Advisor; its findings arrive in a new topic."""
        if not str(msg).strip():
            raise ValueError("message must not be empty")
        me = self._me()
        # The Advisor reads what the asker sees: the same directory.
        props = {"name": "advisor", "preset": "advisor", "parent": me.id, "cwd": me.props["cwd"]}
        advisor, topic = self._spawn(advisor_runtime(me.runtime), props, "Advisor: " + _first_line(msg), msg)
        return report(
            f"Advisor {label(advisor.id)} is considering the question in topic #{topic}. Its "
            "answer will arrive in that topic."
        )

    def cancel(self, *, agent_id):
        """Interrupt an Engineer's current turn."""
        target = resolve(str(agent_id))
        known = _rho.agents()
        if target not in known:
            raise ValueError(f"no agent with id {agent_id} in this workset")
        props = known[target]["props"]
        if props.get("preset") == "advisor":
            raise ValueError("target is not an Engineer")
        if target == self._agent:
            raise ValueError("cannot interrupt yourself")
        # Only the user and the agents it spawned itself interrupt an agent
        # the user manages: the Engineer that started it may still talk to
        # it, but no longer manages it.
        if props.get("started_by") and known[self._agent]["props"].get("parent") != target:
            raise ValueError(f"{label(target)} is managed by the user; only the user can interrupt it")
        _rho.cancel(target)
        return report(
            f"Engineer {label(target)} interrupted. It remains available for follow-up messages."
        )

    def __repr__(self):
        return "<agents>"


class UserAgents(Agents):
    """An Engineer the user manages may also start Engineers for the user."""

    def spawn_user_owned_engineer(self, *, task_name, prompt, workdir=None):
        """Start an Engineer the user manages; it reports to the user, not to
        you. The user learns of it by a mention of its topic."""
        if not str(prompt).strip():
            raise ValueError("prompt must not be empty")
        me = self._me()
        cwd = _workdir(workdir) or me.props["cwd"]
        props = {
            "name": task_name,
            "preset": "engineer",
            "started_by": me.id,
            "cwd": cwd,
        }
        agent, topic = self._spawn(engineer_runtime(me.runtime), props, task_name, prompt, mentions=[(rho_chat.USER, "fyi")])
        return report(
            f'Spawned user-owned Engineer {label(agent.id)} for task "{task_name}" in topic '
            f"#{topic}. It works in {cwd}. It reports to the user, not to you."
        )


#: The longest topic name a tool makes from a message.
TOPIC_NAME_CHARS = 60


def _first_line(text):
    line = next((line.strip() for line in str(text).splitlines() if line.strip()), "")
    return line if len(line) <= TOPIC_NAME_CHARS else line[: TOPIC_NAME_CHARS - 1] + "…"


def _workdir(workdir):
    if workdir is None:
        return None
    workdir = os.fspath(workdir)
    if not os.path.isabs(workdir):
        raise ValueError("workdir must be an absolute path")
    if not os.path.isdir(workdir):
        raise ValueError(f"workdir is not a directory: {workdir}")
    return workdir


class Skills:
    """Load skills from SKILL.md files."""

    def __init__(self, agent: str):
        self._agent = agent

    def load(self, path):
        """Show yourself the skill at `path`, its directory or its SKILL.md.
        A skill whose metadata sets `rho-keep-after-compaction: "true"` is
        read again and shown to you after each compaction."""
        print(rho_skills.load(rho_agents.Agent(self._agent), path))

    def __repr__(self):
        return "<skills>"


def exports(agent) -> dict:
    """The tools for `agent`, by global name. Advisors start no agents."""
    preset = agent.props["preset"]
    tools = {"chat": Chat(agent.id), "skills": Skills(agent.id)}
    if preset == "engineer":
        tools["agents"] = UserAgents(agent.id)
    elif preset == "child":
        tools["agents"] = Agents(agent.id)
    return tools
