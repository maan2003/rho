# rho fork of devenv

This is a hard fork of [devenv](https://github.com/cachix/devenv) (imported
as a squashed subtree at 74075803bf275361b5e3c40f33797a9f308cb54b). Rho keeps
only the Nix C API backend, which evaluates flake `devShells` for
`rho-devshell-builder`; the devenv CLI, module system, processes, tasks,
shell, TUI and evaluation caches were removed. Rho's architecture takes precedence over upstream's; do not
expect `git subtree pull` to apply cleanly.

devenv is licensed under the Apache License 2.0, see `LICENSE`.
