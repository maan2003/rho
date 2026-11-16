# SPEC-shell-address: A shell's address

## Record justification

The address is written by `rho_shell_view::ui::ShellAddress`, kept in the
user's ledger as the key of their facts about it, and read back by every
GUI device and build that shares the ledger, so no one artifact owns it.

## Address

An agent's host-owned shell is `shell:<host>/<agent>`.

- `<host>` is the name the user attached the host under. It may hold `/`.
- `<agent>` is the encoded id of the agent whose workset runs the shell
  (`eng-ht08`); it never holds `/`, so it is read from the right.

An agent has at most one shell, so the agent names it. Opening the
address starts the shell when none runs and attaches to it; closing the
surface only detaches, and the shell keeps running until it is closed on
the host.
