# SPEC-desktop-address: A desktop's address

## Record justification

The address is written by `rho_desktop_client::ui::DesktopAddress`, kept
in the user's ledger as the key of their facts about it, and read back by
every GUI device and build that shares the ledger, so no one artifact owns
it.

## Address

A desktop a host's workset runs is `desktop:<host>/<agent>/<name>`.

- `<host>` is the name the user attached the host under. It may hold `/`.
- `<agent>` is the encoded id of the agent whose workset runs the desktop
  (`eng-ht08`), as the host's desktops stream lists it.
- `<name>` is the desktop's name in that workset, as the stream lists it.

Neither the agent nor the name holds `/`, so both are read from the right;
a listed desktop whose name would hold one has no address and is not
offered.
