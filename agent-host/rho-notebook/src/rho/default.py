"""The default policy, installed before init.py runs. init.py replaces any
part of it by assigning to `rho.hooks` or `rho.workset.namespace`."""

import asyncio
import os
import re
import sys
import traceback

import _rho

from . import agents, hooks, mail, prompt, skills, tools, workset

#: Seconds before a failed delivery is tried again.
RETRY_SECONDS = 5


def install() -> None:
    hooks.instructions = prompt.instructions
    hooks.on_notebook = on_notebook
    hooks.on_compaction = on_compaction
    hooks.on_turn_end = on_turn_end
    hooks.on_agent = on_agent
    workset.namespace["rho"] = sys.modules["rho"]
    workset.namespace["new_agent"] = new_agent


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
    """A child that failed tells its parent; whatever else it has to say it
    mails itself."""
    parent = agent.props.get("parent")
    if outcome["reason"] == "failed" and parent is not None:
        mail.send(
            agent.id,
            parent,
            f"Agent hit an error and stopped: {outcome['error']}",
            thread=mail.thread_with(agent.id, parent),
        )


async def on_agent(agent) -> None:
    await deliver(agent)


async def deliver(agent) -> None:
    """Hand the agent its mail, in order, forever. It resumes after the
    newest mail the agent's log says it was handed."""
    while True:
        try:
            after = _rho.delivered(agent.id)
            while True:
                for record in await mail.inbox(agent.id, after):
                    wake = "person" if record["sender"] == mail.USER else "agent"
                    await asyncio.to_thread(agent.input, render(record), wake=wake, mail=record["id"])
                    after = record["id"]
        except Exception:
            print(f"rho: delivering mail to {agent.id}:", file=sys.stderr)
            traceback.print_exc()
            await asyncio.sleep(RETRY_SECONDS)


def render(record: dict) -> str:
    """A mail as the model reads it."""
    sender = record["sender"]
    if sender == mail.USER:
        return f"Message from the user:\n{record['text']}"
    label = prompt.label(sender)
    text = f"Message Type: MESSAGE\nSender: {label}\nPayload:\n{record['text']}"
    if label.startswith("adv-"):
        text += f"\n\nAdvisor {label} remains available. Use mail.send with this ID to continue."
    return text


def new_agent(*, repo: str | None = None, rev: str = "", cwd: str | None = None, runtime: dict, preset: str = "engineer") -> str:
    """Create an agent the user manages; returns its id. With `repo`, clone
    it at `rev` into the workset as the directory its URL names, and work
    there; otherwise work in `cwd`, or the workset's root. The host mails it
    the user's first message."""
    if preset not in ("engineer", "advisor"):
        raise ValueError(f'preset is "engineer" or "advisor", not {preset!r}')
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
    props = {"preset": preset, "user_facing": "true", "cwd": cwd}
    return agents.create(runtime, props).id
