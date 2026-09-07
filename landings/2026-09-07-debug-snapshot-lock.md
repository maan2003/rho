`rho debug agents`, `context`, and `migrate` now hold the daemon's runtime lock
before byte-copying `rho.redb`; if the daemon is running they refuse with an
instruction to stop it instead of producing a torn copy that redb later reports
as corrupted. A tempdir database test covers both the refusal under a competing
lock and a readable copy once that lock is released.
