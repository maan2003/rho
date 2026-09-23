# rho fork of devenv

This is a hard fork of [devenv](https://github.com/cachix/devenv) (imported
as a squashed subtree at 74075803bf275361b5e3c40f33797a9f308cb54b). Rho keeps
the Nix C API backend, evaluation-effect tracking and SQLite evaluation cache
as the starting point for evaluating flake `devShells` into cached shell
environments; the devenv CLI, module system, processes, tasks, shell and TUI
were removed. Rho's architecture takes precedence over upstream's; do not
expect `git subtree pull` to apply cleanly.

devenv is licensed under the Apache License 2.0, see `LICENSE`.
