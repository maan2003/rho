"""The tools the default policy puts in an agent's notebook: `mail`,
`agents` and `skills`. Rust adds `web`, `view_image`, `papercut`,
`end_turn` and `archive`.

A tool runs on the notebook thread of the agent that calls it. What it
reports, it prints, so the model sees it with its cell, and returns as an
awaitable string, so the model may `await` it or not.
"""

import os

import _rho

from . import agents as rho_agents
from . import mail as rho_mail
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


class Mail:
    """Mail to the user or to another agent."""

    def __init__(self, agent: str):
        self._agent = agent

    def send(self, to, text, *, kind=None):
        """Send `text` to "user" or to an agent id. Mail to the user needs a
        kind: "ask", "result", "fyi" or "status"."""
        text = str(text)
        if not text.strip():
            raise ValueError("a message needs text")
        if to == rho_mail.USER:
            props = rho_agents.Agent(self._agent).props
            if props.get("user_facing") != "true":
                parent = props.get("parent")
                raise ValueError(
                    f"you report to your parent agent {label(parent)}; mail it instead"
                    if parent
                    else "only an agent the user manages can mail the user"
                )
            if kind is None:
                raise ValueError('mail to the user needs kind: "ask", "result", "fyi" or "status"')
            rho_mail.send(self._agent, to, text, kind=kind)
            return Reply("")
        recipient = resolve(str(to))
        if recipient == self._agent:
            raise ValueError("cannot send a message to yourself")
        rho_mail.send(self._agent, recipient, text, kind=kind)
        return report(f"Message sent to {label(recipient)}.")

    def __repr__(self):
        return "<mail>"


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

    def _spawn(self, runtime, props, prompt):
        self._check_depth()
        agent = rho_agents.create(runtime, props)
        rho_mail.send(self._agent, agent.id, prompt)
        return agent

    def spawn_new_engineer(self, *, task_name, prompt, workdir=None):
        """Start an Engineer on `prompt`, in `workdir` or your directory."""
        if not str(prompt).strip():
            raise ValueError("prompt must not be empty")
        me = self._me()
        cwd = _workdir(workdir) or me.props["cwd"]
        props = {"name": task_name, "preset": "child", "parent": me.id, "cwd": cwd}
        child = self._spawn(engineer_runtime(me.runtime), props, prompt)
        return report(
            f'Spawned agent {label(child.id)} for task "{task_name}". It is working now; its '
            f"results will arrive as mail from that Engineer. It works in {cwd}. Use mail.send "
            "to follow up."
        )

    def spawn_new_advisor(self, msg):
        """Ask an independent Advisor; its findings arrive as mail."""
        if not str(msg).strip():
            raise ValueError("message must not be empty")
        me = self._me()
        # The Advisor reads what the asker sees: the same directory.
        props = {"name": "advisor", "preset": "advisor", "parent": me.id, "cwd": me.props["cwd"]}
        advisor = self._spawn(advisor_runtime(me.runtime), props, msg)
        return report(
            f"Advisor {label(advisor.id)} is considering the question. Its answer will arrive "
            "as mail."
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
        """Start an Engineer the user manages as its own thread; it reports
        to the user, not to you."""
        if not str(prompt).strip():
            raise ValueError("prompt must not be empty")
        me = self._me()
        cwd = _workdir(workdir) or me.props["cwd"]
        props = {
            "name": task_name,
            "preset": "engineer",
            "started_by": me.id,
            "user_facing": "true",
            "cwd": cwd,
        }
        agent = self._spawn(engineer_runtime(me.runtime), props, prompt)
        return report(
            f'Spawned user-owned Engineer {label(agent.id)} for task "{task_name}". It works in '
            f"{cwd}. It appears in the user's agent list and reports to the user, not to you."
        )


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
    tools = {"mail": Mail(agent.id), "skills": Skills(agent.id)}
    if preset == "engineer":
        tools["agents"] = UserAgents(agent.id)
    elif preset == "child":
        tools["agents"] = Agents(agent.id)
    return tools
