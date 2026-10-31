"""Skills an agent loads, and the ones it keeps after a compaction.

An agent keeps the skills whose SKILL.md asks, in its `skills` prop: their
directories, one per line, in the order loaded. The prop survives a
restart; the text is read again each time it is shown.
"""

import os
import re

import yaml

PROP = "skills"
#: The `metadata` key a skill sets to "true" to be shown again after each
#: compaction, in the map the Agent Skills spec reserves for clients.
KEEP_AFTER_COMPACTION = "rho-keep-after-compaction"


def kept(agent) -> list[str]:
    value = agent.props.get(PROP, "")
    return value.split("\n") if value else []


def render(directory: str) -> str:
    """The skill as the model is shown it, as it is on disk now."""
    try:
        text = _read(directory)
    except OSError as error:
        return f'<skill path="{directory}" error="{error}" />'
    return f'<skill path="{directory}">\n{text.rstrip()}\n</skill>'


def load(agent, path) -> str:
    """Show `agent` the skill at `path`, its directory or its SKILL.md, and
    keep it if it asks."""
    directory = os.path.abspath(os.fspath(path))
    if os.path.basename(directory) == "SKILL.md":
        directory = os.path.dirname(directory)
    try:
        text = _read(directory)
    except OSError as error:
        raise ValueError(f"read {os.path.join(directory, 'SKILL.md')}: {error}") from None
    if keeps_after_compaction(text):
        dirs = kept(agent)
        if directory not in dirs:
            agent.set_props({**agent.props, PROP: "\n".join([*dirs, directory])})
    return f'<skill path="{directory}">\n{text.rstrip()}\n</skill>'


def _read(directory: str) -> str:
    with open(os.path.join(directory, "SKILL.md"), encoding="utf-8") as file:
        return file.read()


class _Loader(yaml.SafeLoader):
    """YAML 1.2 booleans, as the Rust side reads front matter: `yes` and
    `on` stay strings."""


_Loader.yaml_implicit_resolvers = {
    first: [(tag, regexp) for tag, regexp in resolvers if tag != "tag:yaml.org,2002:bool"]
    for first, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()
}
_Loader.add_implicit_resolver(
    "tag:yaml.org,2002:bool",
    re.compile(r"^(?:true|True|TRUE|false|False|FALSE)$"),
    list("tTfF"),
)


def keeps_after_compaction(content: str) -> bool:
    """Whether the skill whose SKILL.md is `content` asks to be shown again
    after each compaction: `metadata.rho-keep-after-compaction` is "true"."""
    front = _front_matter(content)
    if front is None:
        return False
    try:
        fields = yaml.load(front, Loader=_Loader)
    except yaml.YAMLError:
        return False
    if not isinstance(fields, dict) or not isinstance(fields.get("metadata"), dict):
        return False
    value = fields["metadata"].get(KEEP_AFTER_COMPACTION)
    if isinstance(value, bool):
        value = "true" if value else "false"
    return value == "true"


def _front_matter(content: str) -> str | None:
    content = content.removeprefix("\ufeff")
    if not content.startswith("---"):
        return None
    rest = content[3:]
    if rest.startswith("\n"):
        rest = rest[1:]
    elif rest.startswith("\r\n"):
        rest = rest[2:]
    else:
        return None
    pos = 0
    for line in rest.split("\n"):
        if line.rstrip("\r").rstrip() == "---":
            return rest[:pos]
        pos += len(line) + 1
    return None
