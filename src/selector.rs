//! A [`PathSelector`] that prefers the IPC transport for co-located peers.

use std::time::Duration;

use iroh::endpoint::transports::{Addr, PathSelection, PathSelectionContext, PathSelector};

use crate::IPC_TRANSPORT_ID;

/// How much direct IPv6 paths are preferred over IPv4, expressed as an RTT
/// advantage. Parity with iroh's default `BiasedRttPathSelector`.
const IPV6_RTT_ADVANTAGE: Duration = Duration::from_millis(3);

/// Minimum biased-RTT improvement before switching away from the currently
/// selected path within the same tier. Parity with iroh's default selector:
/// without this hysteresis, near-equal paths (a peer with several good
/// routes) flap selection on RTT jitter, and every selection change is a
/// path migration for in-flight streams.
const RTT_SWITCHING_MIN: Duration = Duration::from_millis(5);

/// Biased RTT assigned to a path whose stats cannot be read (e.g. it is
/// being torn down concurrently with selection). Ranks last within its
/// tier without affecting tier ordering.
const UNKNOWN_RTT_NS: i128 = i128::MAX / 2;

/// A [`PathSelector`] that selects the IPC (local-socket) path whenever one
/// is open to the peer, and otherwise behaves like iroh's default selector.
///
/// Selection ranks paths by tier, then by biased RTT within the tier:
///
/// 1. **IPC** — any path on this crate's custom transport. A co-located
///    peer's socket path always wins: RTT alone cannot make that call,
///    because on the same machine *every* path has microsecond-scale RTT
///    and a fixed bias (like the default selector's IPv6 advantage) dwarfs
///    the real differences.
/// 2. **Direct** — UDP paths (IPv6 with a 3 ms RTT advantage over IPv4,
///    matching iroh's default) and custom transports other than IPC.
/// 3. **Relay** — backup only: never selected while any higher-tier path
///    is open, no matter its RTT.
///
/// Within a tier, the currently selected path is sticky: a candidate must
/// improve on it by at least 5 ms of biased RTT before selection switches
/// (anti-flap, matching iroh's default). Across tiers, switching is
/// immediate — the moment an IPC path opens it takes over, and if the
/// socket goes away iroh falls back to the best remaining path.
///
/// Install via
/// [`Builder::path_selector`](iroh::endpoint::Builder::path_selector).
#[derive(Debug, Default, Clone, Copy)]
pub struct PreferIpcTransport;

impl PathSelector for PreferIpcTransport {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let paths: Vec<_> = ctx.paths().collect();
        let keys: Vec<RankKey> = paths
            .iter()
            .map(|p| RankKey::rank(&p.network_path().remote(), p.stats().map(|s| s.rtt)))
            .collect();
        let current = ctx
            .current()
            .and_then(|cur| paths.iter().position(|p| p.network_path() == cur));

        let mut selection = PathSelection::none();
        if let Some(index) = pick_index(current, &keys) {
            selection.set(&paths[index]);
        }
        selection
    }
}

/// Path preference tier: lower is better. Within a tier, biased RTT
/// decides; across tiers, RTT is irrelevant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// A path on this crate's IPC transport — co-located, always preferred.
    Ipc,
    /// A direct path: UDP (v4/v6) or a non-IPC custom transport.
    Direct,
    /// A relayed path — backup only.
    Relay,
}

/// Sort key for one candidate path: `(tier, biased RTT)`, lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    tier: Tier,
    biased_rtt_ns: i128,
}

impl RankKey {
    /// Classify a path by its remote address and (optional) measured RTT.
    fn rank(remote: &Addr, rtt: Option<Duration>) -> Self {
        let (tier, bias_ns) = match remote {
            Addr::Custom(c) if c.id() == IPC_TRANSPORT_ID => (Tier::Ipc, 0),
            Addr::Custom(_) => (Tier::Direct, 0),
            Addr::Ip(addr) if addr.is_ipv6() => {
                (Tier::Direct, -(IPV6_RTT_ADVANTAGE.as_nanos() as i128))
            }
            Addr::Ip(_) => (Tier::Direct, 0),
            Addr::Relay(..) => (Tier::Relay, 0),
        };
        let biased_rtt_ns = match rtt {
            Some(rtt) => (rtt.as_nanos() as i128).saturating_add(bias_ns),
            None => UNKNOWN_RTT_NS,
        };
        Self {
            tier,
            biased_rtt_ns,
        }
    }
}

/// The pure selection decision: which candidate (by index) should be
/// selected, or `None` to keep the current selection unchanged.
///
/// - No current selection (or the current path is no longer open): the
///   best-ranked candidate is selected.
/// - A candidate in a better tier than the current path wins immediately.
/// - A candidate in the same tier wins only if its biased RTT improves on
///   the current path's by at least [`RTT_SWITCHING_MIN`].
fn pick_index(current: Option<usize>, keys: &[RankKey]) -> Option<usize> {
    let best = (0..keys.len()).min_by_key(|&i| keys[i])?;
    let Some(current) = current else {
        return Some(best);
    };
    if best == current {
        return None;
    }
    let (best_key, current_key) = (keys[best], keys[current]);
    if best_key.tier < current_key.tier {
        return Some(best);
    }
    let improvement_ns = current_key.biased_rtt_ns - best_key.biased_rtt_ns;
    if best_key.tier == current_key.tier && improvement_ns >= RTT_SWITCHING_MIN.as_nanos() as i128 {
        return Some(best);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipc(rtt_us: u64) -> RankKey {
        RankKey {
            tier: Tier::Ipc,
            biased_rtt_ns: i128::from(rtt_us) * 1_000,
        }
    }

    fn direct(biased_rtt_us: i64) -> RankKey {
        RankKey {
            tier: Tier::Direct,
            biased_rtt_ns: i128::from(biased_rtt_us) * 1_000,
        }
    }

    fn relay(rtt_us: u64) -> RankKey {
        RankKey {
            tier: Tier::Relay,
            biased_rtt_ns: i128::from(rtt_us) * 1_000,
        }
    }

    #[test]
    fn ipc_beats_lower_rtt_direct() {
        // Same-host reality: a UDP path can report a lower RTT than the
        // socket path; the IPC tier must win regardless.
        assert_eq!(pick_index(None, &[direct(100), ipc(500)]), Some(1));
    }

    #[test]
    fn ipc_opening_preempts_current_direct_immediately() {
        // Cross-tier switching has no hysteresis: the moment the socket
        // path opens it takes over from a selected UDP path.
        assert_eq!(pick_index(Some(0), &[direct(100), ipc(900)]), Some(1));
    }

    #[test]
    fn relay_never_selected_over_direct() {
        assert_eq!(pick_index(None, &[relay(100), direct(50_000)]), Some(1));
    }

    #[test]
    fn relay_selected_when_it_is_all_there_is() {
        assert_eq!(pick_index(None, &[relay(20_000)]), Some(0));
    }

    #[test]
    fn direct_appearing_preempts_current_relay() {
        assert_eq!(pick_index(Some(0), &[relay(100), direct(50_000)]), Some(1));
    }

    #[test]
    fn same_tier_switch_requires_hysteresis_margin() {
        // 3 ms better than current: below the 5 ms threshold — keep.
        assert_eq!(pick_index(Some(0), &[direct(10_000), direct(7_000)]), None);
        // 6 ms better than current: switch.
        assert_eq!(
            pick_index(Some(0), &[direct(10_000), direct(4_000)]),
            Some(1)
        );
    }

    #[test]
    fn keeping_current_returns_none() {
        assert_eq!(pick_index(Some(1), &[direct(9_000), direct(8_000)]), None);
    }

    #[test]
    fn no_current_selects_best() {
        assert_eq!(pick_index(None, &[direct(9_000), direct(8_000)]), Some(1));
    }

    #[test]
    fn empty_candidates_select_nothing() {
        assert_eq!(pick_index(None, &[]), None);
    }

    #[test]
    fn v6_advantage_applies_via_rank() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let v4 = RankKey::rank(
            &Addr::Ip(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 1)),
            Some(Duration::from_millis(2)),
        );
        let v6 = RankKey::rank(
            &Addr::Ip(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 1)),
            Some(Duration::from_millis(4)),
        );
        // 4 ms − 3 ms advantage = 1 ms biased: the v6 path outranks v4@2 ms.
        assert!(v6 < v4);
    }

    #[test]
    fn missing_stats_rank_last_within_tier_but_keep_tier() {
        let ipc_no_stats = RankKey::rank(
            &iroh_base::CustomAddr::from_parts(crate::IPC_TRANSPORT_ID, &[1, 1, 1]).into(),
            None,
        );
        assert_eq!(ipc_no_stats.tier, Tier::Ipc);
        // Even with unknown RTT, the IPC tier outranks any direct path.
        assert!(ipc_no_stats < direct(1));
    }
}
