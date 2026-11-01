"""The default policy, installed before init.py runs. init.py replaces any
part of it by assigning to `rho.hooks` or `rho.workset.namespace`."""

import asyncio
import os
import re
import sys
import traceback

import _rho

from . import agents, chat, hooks, prompt, skills, tools, workset

#: Seconds before a failed delivery is tried again.
RETRY_SECONDS = 5


def install() -> None:
    hooks.instructions = prompt.instructions
    hooks.on_notebook = on_notebook
    hooks.on_compaction = on_compaction
    hooks.on_turn_end = on_turn_end
    hooks.on_agent = on_agent
    hooks.joins = joins
    workset.namespace["rho"] = sys.modules["rho"]
    workset.namespace["new_agent"] = new_agent
    workset.namespace["join"] = chat.join


def on_notebook(agent, ns) -> None:
    ns.update(tools.exports(agent))
    os.chdir(agent.props["cwd"])


def on_compaction(agent) -> list[str]:
    """The skills the agent keeps, as they read now, in one text."""
    kept = [skills.render(directory) for directory in skills.kept(agent)]
    if not kept:
        return []
    return ["Your loaded skills, as they are now. Keep following them:\n\n" + "\n\n".join(kept)]


def on_turn_end(agent, outcome) -> None:
    """A child that failed tells its parent, in their direct conversation;
    whatever else it has to say it posts itself."""
    parent = agent.props.get("parent")
    if outcome["reason"] == "failed" and parent is not None:
        chat.post(agent.id, chat.direct(agent.id, parent), f"Agent hit an error and stopped: {outcome['error']}")


async def on_agent(agent) -> None:
    await deliver(agent)


async def deliver(agent) -> None:
    """Hand the agent what it receives, in order, forever. It resumes after
    the newest message the agent's log says it was handed. An agent started
    from a message first joins its topic and is handed that message."""
    while True:
        try:
            after = _rho.delivered(agent.id)
            if after == 0 and (origin := chat.origin_message(agent.props)) is not None:
                record = chat.message(origin)
                await hand(agent, record)
                after = record["id"]
            while True:
                for record in await chat.inbox(agent.id, after):
                    await hand(agent, record)
                    after = record["id"]
        except Exception:
            print(f"rho: delivering messages to {agent.id}:", file=sys.stderr)
            traceback.print_exc()
            await asyncio.sleep(RETRY_SECONDS)


def joins(agent, record: dict) -> bool:
    """An agent follows the topic it was started from, and a topic where
    the user mentions it. A mention by an agent only reaches it."""
    if record["id"] == chat.origin_message(agent.props):
        return True
    return record["sender"] == chat.USER and any(address == agent.id for address, _ in record["mentions"])


async def hand(agent, record: dict) -> None:
    """Hand one message to the agent, after it joins the message's topic if
    `hooks.joins` says so."""
    if chat.topic(record["topic"])["stream"] is not None and hooks.joins(agent, record):
        chat.join(record["topic"], agent.id)
    wake = "person" if record["sender"] == chat.USER else "agent"
    await asyncio.to_thread(agent.input, render(record, agent.id), wake=wake, message=record["id"])


def render(record: dict, reader: str) -> str:
    """A message as `reader` reads it: its conversation first, so the model
    knows where to answer."""
    topic = chat.topic(record["topic"])
    if topic["stream"] is None:
        other = next(member for member in topic["agents"] if member != reader)
        where = f"[#{record['topic']} direct with {prompt.label(other)}]"
    else:
        where = f"[#{record['topic']} {topic['stream']} › {topic['name']}]"
    sender = record["sender"]
    if sender == chat.USER:
        return f"{where} Message from the user:\n{record['text']}"
    label = prompt.label(sender)
    return f"{where}\nMessage Type: MESSAGE\nSender: {label}\nPayload:\n{record['text']}"


def new_agent(*, repo: str | None = None, rev: str = "", cwd: str | None = None, runtime: dict, preset: str = "engineer", origin: str | None = None) -> str:
    """Create an agent the user manages; returns its id. With `repo`, clone
    it at `rev` into the workset as the directory its URL names, and work
    there; otherwise work in `cwd`, or the workset's root. With `origin`,
    `chat:<message id>`, the agent joins that message's topic and starts
    from it."""
    if preset not in ("engineer", "advisor"):
        raise ValueError(f'preset is "engineer" or "advisor", not {preset!r}')
    props = {"preset": preset}
    if origin is not None:
        props[chat.ORIGIN] = origin
        message = chat.origin_message(props)
        if chat.message(message) is None:
            raise ValueError(f"no message {message} for origin {origin}")
    if repo is not None:
        if cwd is not None:
            raise ValueError("give repo or cwd, not both")
        name = re.split(r"[/:]", repo.rstrip("/"))[-1].removesuffix(".git")
        if not name:
            raise ValueError(f"no directory name in {repo!r}")
        cwd = os.path.join(workset.root, name)
        workset.clone(repo, cwd, rev)
    elif cwd is None:
        cwd = workset.root
    if not os.path.isabs(cwd) or not os.path.isdir(cwd):
        raise ValueError(f"cwd is not an absolute directory: {cwd}")
    props["cwd"] = cwd
    return agents.create(runtime, props).id
