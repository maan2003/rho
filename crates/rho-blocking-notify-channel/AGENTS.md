# rho-blocking-notify-channel agent notes

Before changing this crate, read:

- `README.md` for the public channel contract and examples.
- `SECURITY.md` for reliability-sensitive synchronization invariants.

Preserve the coalesced one-bit MPSC semantics, pending-notification-before-disconnect
ordering, receiver-drop behavior, and `Receiver: Send + !Sync` auto-trait contract.
Update docs and focused tests whenever those semantics change.
