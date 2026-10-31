"""The default instructions: a whole document per role and model family,
with the agent's team, the section only an agent the user manages gets, and
its workspace and project guidance filled in."""

import functools
import os
import pathlib
import sys

import _rho

from . import workset

_DIR = pathlib.Path(__file__).with_name("prompt")

#: The template family each runtime reads: the native runtime runs GPT
#: models; Claude Code gets the Claude family as its system prompt.
FAMILIES = {"native": "gpt", "claude": "claude"}


@functools.cache
def template(family: str, name: str) -> str:
    return (_DIR / family / f"{name}.md").read_text(encoding="utf-8")


def render(template: str, team: str, user_owned: str, context: str) -> str:
    """Fill a template's slots. The context goes in last, so project
    guidance that happens to spell a slot stays as written."""
    return (
        template.replace("{{team}}", team)
        .replace("{{user_owned_engineers}}", user_owned)
        .replace("{{context}}", context)
    )


def label(agent_id: str) -> str:
    """How models and the GUI name an agent: its id behind "adv-" for an
    Advisor and "eng-" otherwise. An agent of another workset has no known
    role, so it is its bare id."""
    spec = _rho.agents().get(agent_id)
    if spec is None:
        return agent_id
    prefix = "adv" if spec["props"].get("preset") == "advisor" else "eng"
    return f"{prefix}-{agent_id}"


def started_by_note(by: str) -> str:
    return (
        f"Engineer {by} started you for the user; ask it with mail.send if you need more "
        "context."
    )


def team(agent_id: str, props: dict) -> str:
    """Who the agent is among the others, and how they talk."""
    me = label(agent_id)
    parent = props.get("parent")
    if parent is not None:
        identity = (
            "You are an agent in a team of agents collaborating to complete a task. Your "
            f"agent id is {me}; your parent agent is {label(parent)}.\n\nMessages from your "
            "parent define your task. Report to your parent with mail.send."
        )
    else:
        identity = (
            "You are the primary agent in a team of agents collaborating to fulfill the "
            f"user's goals. Your agent id is {me}."
        )
    preset = props["preset"]
    if preset == "advisor":
        return (
            f"{identity}\n\n"
            "Complete your independent analysis and send it to your parent with mail.send. "
            "Ask a known agent for context the same way, and end your turn when blocked on a reply.\n"
        )
    started_by = props.get("started_by")
    if preset == "child":
        ownership = (
            "You were spawned by another Engineer. Own the bounded assignment in the "
            "parent message."
        )
    elif started_by is not None:
        # An Engineer started for the user is the user's like any other; it
        # only learns who holds the context behind its brief.
        ownership = f"{started_by_note(label(started_by))} You own the user's technical outcome."
    else:
        ownership = "You were started directly and own the user's technical outcome."
    return f"""{identity}

{ownership}

You will receive agent messages in this format:
```
Message Type: MESSAGE
Sender: <agent id>
Payload:
<payload text>
```

Use `mail.send` for bidirectional communication with any known agent.

"""


def workspace(root: str, cwd: str, git: bool) -> str:
    out = f"""## Workspace Context

Working directory: {cwd}

Your workset is the directory {root}, holding the repositories you work in. Relative paths in \
commands and patches resolve against your working directory. Stay within the workset unless the \
user points you elsewhere.

Clone further repositories into the workset with \
`git clone <url>` (fast: clones are born from a local mirror, and `git fetch` reads it), and \
add checkouts of a repository with `git worktree add`. Nothing in the workset is cleaned up \
behind you; what is there when you start is the starting state you were given.

"""
    if git:
        out += "This repository is a git checkout; `origin` is the real remote.\n\n"
    return out


def agents_md(files: list[dict]) -> str:
    if not files:
        return ""
    out = "## AGENTS.md instructions\n"
    out += "The following instructions were loaded from AGENTS.md files. They are user/project instructions: follow them unless they conflict with higher-priority system or developer instructions. More specific files appear later and usually override broader ones.\n\n"
    for file in files:
        out += f'<AGENTS_FILE path="{file["path"]}">\n'
        out += file["content"]
        if not file["content"].endswith("\n"):
            out += "\n"
        out += "</AGENTS_FILE>\n\n"
    return out


def skills(found: list[dict]) -> str:
    """The skill catalogue, each path short behind an alias of the
    directory its skill directory is in."""
    if not found:
        return ""
    roots: list[str] = []
    entries = ""
    for skill in sorted(found, key=lambda skill: skill["name"]):
        path = pathlib.PurePosixPath(skill["path"])
        root = path.parent.parent if path.parent != path.parent.parent else None
        if root is not None:
            if str(root) not in roots:
                roots.append(str(root))
            shown = f"r{roots.index(str(root))}/{path.relative_to(root)}"
        else:
            shown = str(path)
        entries += f"- {skill['name']}: {skill['description']} (file: {shown})\n"
    out = "## Skills\n\nA skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and a path. Short paths can be expanded into absolute paths using the skill roots table.\n\nTo use a skill, call `skills.load(path)` with its directory or its `SKILL.md`, not a plain file read. It shows you the skill. A skill can ask to be shown again after each compaction.\n\n"
    if roots:
        out += "### Skill roots\n\n"
        for index, root in enumerate(roots):
            out += f"- `r{index}` = `{root}`\n"
        out += "\n"
    out += "### Available skills\n\n" + entries + "\n"
    return out


def in_checkout(root: str, cwd: str) -> bool:
    """Whether `cwd` is inside a git checkout that counts: in the workset,
    only one that begins within it."""
    current = os.path.realpath(cwd)
    while True:
        if os.path.exists(os.path.join(current, ".git")):
            break
        parent = os.path.dirname(current)
        if parent == current:
            return False
        current = parent
    if _within(cwd, root) and not _within(current, root):
        return os.path.exists(os.path.join(cwd, ".git"))
    return True


def _within(path: str, root: str) -> bool:
    return pathlib.PurePosixPath(path).is_relative_to(root)


def context(cwd: str) -> str:
    """The workspace section, AGENTS.md guidance and skill catalogue for an
    agent working in `cwd`."""
    try:
        found = _rho.context(cwd)
        git = in_checkout(workset.root, cwd)
    except Exception as error:
        # A working directory that went away still gets its instructions.
        print(f"rho: context discovery for {cwd}: {error}", file=sys.stderr)
        found, git = {"agents_files": [], "skills": []}, False
    return workspace(workset.root, cwd, git) + agents_md(found["agents_files"]) + skills(found["skills"])


def instructions(agent) -> str:
    """The default instructions for `agent`, from its runtime and its
    `preset`, `parent`, `started_by` and `cwd` props."""
    spec = agent.spec
    props = spec["props"]
    family = FAMILIES[spec["runtime"]["kind"]]
    preset = props["preset"]
    user_owned = template(family, "user-owned-engineers") if preset == "engineer" else ""
    return render(template(family, preset), team(agent.id, props), user_owned, context(props["cwd"]))
