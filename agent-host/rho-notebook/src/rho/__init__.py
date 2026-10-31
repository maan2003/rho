"""rho2's policy layer: the Python each workset process runs ([ARCH-rho2]).

Rust is the core; this package decides policy on top of the native `_rho`
module: which agents exist and on which runtime, what each model is told,
and how mail reaches agents. Policy code and `~/.config/rho/init.py` use
the public names here; Rust calls the boot functions `_main`, `_eval`,
`_hook` and `_on_notebook` (see `rho._boot`).
"""

from . import agents, hooks, mail, workset
from ._boot import _eval, _hook, _main, _on_notebook
from .agents import Agent

agent = Agent

__all__ = ["Agent", "agent", "agents", "hooks", "mail", "workset"]
