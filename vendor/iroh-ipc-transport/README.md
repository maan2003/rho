# iroh-ipc-transport

[![Crates.io](https://img.shields.io/crates/v/iroh-ipc-transport.svg)](https://crates.io/crates/iroh-ipc-transport)
[![Documentation](https://docs.rs/iroh-ipc-transport/badge.svg)](https://docs.rs/iroh-ipc-transport)
[![CI](https://github.com/legra-ai/iroh-ipc-transport/actions/workflows/ci.yml/badge.svg)](https://github.com/legra-ai/iroh-ipc-transport/actions/workflows/ci.yml)

Local communication between processes through one native [iroh](https://iroh.computer)
QUIC connection. The crate owns platform selection, local discovery, exclusive
endpoint ownership and cleanup. QUIC supplies authenticated identity, reliable
streams, multiplexing and congestion control.

| Platform | Backend |
| --- | --- |
| macOS | IPv4 loopback UDP, using Noq's batched I/O |
| Linux | Bound Unix datagrams |
| Windows | Local message-mode named pipes, rejecting remote clients |

macOS deliberately never creates AF_UNIX datagrams, including during discovery
and cleanup. This avoids that kernel subsystem's disconnect path. UDP remains
an explicit custom transport, so `PreferIpcTransport` can prefer it over ordinary
IP and relay paths. The backend does not change after an error.

Use `resolve_target` before every dial: a live local lease produces a target
containing only the local custom address. iroh 1.3 can leave a custom path
unopened when an IP path wins its initial handshake race; a selector cannot
select an unopened path. Choosing a local-only target prevents that race.
Without a local lease, the original remote target is preserved. There is no
remote retry after a local transport failure.

## Usage

```rust,no_run
use std::sync::Arc;
use iroh::{Endpoint, EndpointAddr, SecretKey, endpoint::presets};
use iroh_ipc_transport::{IpcTransport, LocalDiscovery, PreferIpcTransport};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let discovery = LocalDiscovery::new(std::env::temp_dir().join("my-app-ipc"));
let key = SecretKey::generate();
let transport = IpcTransport::prepare(discovery.clone(), key.public()).await?;
let endpoint = Endpoint::builder(presets::Minimal)
    .secret_key(key)
    .add_custom_transport(Arc::new(transport.clone()))
    .path_selector(Arc::new(PreferIpcTransport))
    .address_lookup(discovery.clone())
    .bind()
    .await?;

// Resolve before every dial, including targets already carrying IP/relay addresses.
// A live local lease selects the local backend deterministically.
# let peer_id = SecretKey::generate().public();
let target = discovery.resolve_target(EndpointAddr::new(peer_id)).await?;

endpoint.close().await;
transport.close().await?;
# Ok(())
# }
```

`LocalDiscovery::new` creates a missing rendezvous directory owner-only
(`0700`); `LocalDiscovery::shared` creates it world-writable and sticky
(`01777`) so peers running as different users can rendezvous. Both set the mode
explicitly, independent of the umask, and never change an existing directory.

Applications choose a shared local rendezvous directory and endpoint identity;
they do not construct socket paths, UDP ports or pipe names. Use a directory
writable by the intended cooperating accounts. Unix socket and record modes
are `0666`; QUIC/TLS, rather than filesystem reachability, authenticates peers.
On Windows the directory and named-pipe ACLs must allow those accounts access.
The default pipe security descriptor permits the owning account and system;
this crate does not broaden Windows ACLs.

`prepare` binds and publishes asynchronously. A second live endpoint with the
same identity fails with `AddrInUse`. Its factory can bind only once. Leases
are OS file locks; lookup never connects a socket or checks a PID to determine
liveness. A process crash releases its lease, making its record undiscoverable.
A later owner of the same identity can reclaim it. One persistent directory
lock prevents unlink/rebind races between processes.

After closing iroh, `transport.close().await` closes the OS resources before
removing discovery records. Closing again cannot remove a replacement owner's
record. Dropping the final transport/sender/receiver handle schedules cleanup;
when the runtime is already gone, an unlocked record may remain and is ignored.

Windows uses a fixed-size routing token and a bounded length field inside each
native pipe message. Tokio/Mio can deliver a message in partial reads, so the
receiver reassembles it before passing one datagram to QUIC. This is transport
framing, independent of the application protocol. Each endpoint permits at most
128 active inbound and 128 cached outbound pipes, with a 32-packet receive queue
and a 65,535-byte datagram limit. Idle outbound handles expire after 60 seconds
when admitting another peer; exhausting the active limit returns an error.

## Validation and maintenance

Requires iroh 1.3 and its experimental `unstable-custom-transports` feature.
The custom address format and preparation API replace the earlier pathname API.

`cargo test --locked` covers local-only QUIC echo, burst transfer, coexistence
with IP paths, lease collisions, process-death recovery, malformed addresses and
partial pipe reads. Native Linux, macOS and Windows tests, Clippy, documentation
and the distributable crate archive gate publication through the shared
`legra-ai/github-actions` maintenance workflow. Compiler and formatter versions
are pinned separately. Run `sh scripts/fmt.sh` to format.

Dependabot merges remain manual: the organization's dedicated dependency-merger
App currently grants access only to its two pilot repositories. Release
publication uses the separate release-writer identity. This repository does not
reuse that writer to bypass merge checks or broaden the merger's credential scope.

Transfer tests are correctness and progress checks. They are not Host/Custodian
durability benchmarks and do not establish a performance ranking between OSes.

## License

Copyright © 2026 DataRoad Inc, Delaware, USA, trading as Legra.

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your
option. Contributions are licensed under the same terms.
