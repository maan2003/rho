//! Bound work across capture, local relay, WAN, and receiver, not just a
//! socket. Checkpoints stay reliable; replaceable states retain conservative
//! byte debt until a newer receipt, so cancellation cannot create endless work.
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use rho_desktop_proto::Feedback;

use crate::FrameKind;

struct Viewer {
    feedback: Feedback,
    seen: Instant,
    base_rtt_us: u64,
}

struct Pending {
    timestamp_us: u64,
    bytes: u64,
    encoded: Instant,
    refinement: bool,
    kind: FrameKind,
}

/// Shared by the source encoder and its control connections.
pub struct Sender {
    viewers: BTreeMap<u64, Viewer>,
    pending: VecDeque<Pending>,
    bitrate: u32,
    adjusted: Option<Instant>,
}

/// Admission and telemetry for one source; all active viewers must keep up.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub bitrate: u32,
    pub outstanding_bytes: u64,
    pub window_bytes: u64,
    pub oldest_us: u64,
    pub ready: bool,
    pub refine: bool,
    pub decode_us: u64,
}

impl Default for Sender {
    fn default() -> Self {
        Self {
            viewers: BTreeMap::new(),
            pending: VecDeque::new(),
            bitrate: 2_000_000,
            adjusted: None,
        }
    }
}

impl Sender {
    /// Explicit initial/user target; subsequent feedback adapts production.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.bitrate = bitrate.clamp(128_000, 4_000_000);
    }

    /// Starts a new producer lifetime without retaining prior frame identities.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.viewers.clear();
        self.adjusted = None;
    }

    /// Returns true only for a new recovery generation, never its repeated
    /// reports.
    pub fn feedback(&mut self, id: u64, feedback: Feedback, now: Instant) -> bool {
        let previous = self.viewers.get(&id);
        let recovery = feedback.recover
            && previous.is_none_or(|v| {
                !v.feedback.recover || v.feedback.recovery_id != feedback.recovery_id
            });
        let progressed = feedback.received.is_some()
            && previous.is_none_or(|v| feedback.received > v.feedback.received);
        let base_rtt_us = previous.map_or(feedback.rtt_us, |v| {
            if feedback.rtt_us == 0 {
                v.base_rtt_us
            } else if v.base_rtt_us == 0 {
                feedback.rtt_us
            } else {
                v.base_rtt_us.min(feedback.rtt_us)
            }
        });
        self.viewers.insert(
            id,
            Viewer {
                feedback,
                seen: now,
                base_rtt_us,
            },
        );
        self.prune();

        // A production controller, not a second packet congestion controller.
        // Do not probe during idle periods or turn slow image serialization
        // into congestion. Backlog pressure reduces encoding, not
        // transport draining.
        if progressed
            && self
                .adjusted
                .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
        {
            let budget = self.budget(now);
            let capacity = self
                .viewers
                .values()
                .filter_map(|v| (v.feedback.delivery_bps > 0).then_some(v.feedback.delivery_bps))
                .min();
            if budget.outstanding_bytes > budget.window_bytes
                && self.pending.iter().any(|p| !p.refinement)
            {
                let target = capacity
                    .unwrap_or(u64::from(self.bitrate))
                    .saturating_mul(85)
                    / 100;
                self.bitrate = (u64::from(self.bitrate) * 3 / 4)
                    .min(target)
                    .clamp(128_000, 4_000_000) as u32;
            } else if self.pending.iter().any(|p| !p.refinement) {
                self.bitrate = (self.bitrate + self.bitrate / 20).min(4_000_000);
            }
            self.adjusted = Some(now);
        }
        recovery
    }

    pub fn remove(&mut self, id: u64) {
        self.viewers.remove(&id);
        self.prune();
    }

    fn prune(&mut self) {
        let received = self
            .viewers
            .values()
            .map(|v| v.feedback.received.map_or(0, |id| id.timestamp_us))
            .min();
        if let Some(received) = received {
            while self
                .pending
                .front()
                .is_some_and(|p| p.timestamp_us <= received)
            {
                self.pending.pop_front();
            }
        }
    }

    /// Account at encoder output, before either relay can buffer the bytes.
    pub fn encoded(
        &mut self,
        timestamp_us: u64,
        bytes: usize,
        kind: FrameKind,
        refinement: bool,
        now: Instant,
    ) {
        self.pending.push_back(Pending {
            timestamp_us,
            bytes: bytes as u64,
            encoded: now,
            refinement,
            kind,
        });
    }

    pub fn budget(&self, now: Instant) -> Budget {
        let outstanding_bytes: u64 = self.pending.iter().map(|p| p.bytes).sum();
        let oldest_us = self.pending.front().map_or(0, |p| {
            now.saturating_duration_since(p.encoded).as_micros() as u64
        });
        let mut window_bytes = if self.viewers.is_empty() {
            u64::from(self.bitrate) / 8 * 300 / 1000
        } else {
            u64::MAX
        };
        let mut ready = true;
        let mut age_limit_us = if self.viewers.is_empty() {
            300_000
        } else {
            u64::MAX
        };
        let mut decode_us = 0;
        for viewer in self.viewers.values() {
            let f = &viewer.feedback;
            let rtt = if viewer.base_rtt_us == 0 {
                100_000
            } else {
                viewer.base_rtt_us.max(1_000)
            };
            age_limit_us = age_limit_us.min(rtt.saturating_add(200_000));
            let bytes_per_second = if f.delivery_bps == 0 {
                u64::from(self.bitrate) / 8
            } else {
                f.delivery_bps / 8
            }
            .max(1);
            // One bandwidth-delay product plus 100ms, not one frame per RTT.
            let window = bytes_per_second.saturating_mul(rtt.saturating_add(100_000)) / 1_000_000;
            window_bytes = window_bytes.min(window.max(16 * 1024));
            // Receiver CPU pressure is distinct from transport receipt.
            decode_us = decode_us.max(f.decode_us);
            if let (Some(received), Some(presented)) = (f.received, f.presented)
                && received.timestamp_us.saturating_sub(presented.timestamp_us) > 200_000
            {
                ready = false;
            }
            // A stalled feedback channel must pause production, not silently
            // expire the slow viewer and release unbounded work.
            if now.saturating_duration_since(viewer.seen).as_micros() as u64
                > f.rtt_us.saturating_mul(4).max(1_000_000)
            {
                ready = false;
            }
        }
        // An indivisible frame may exceed the window once. It must finish
        // before further images are encoded (except the one refinement
        // replacement). One fresh motion state may replace a slow
        // static refinement. It cannot abandon a checkpoint, and its
        // own motion debt prevents endless resets.
        let replace_refinement = !self.pending.is_empty()
            && self
                .pending
                .iter()
                .all(|p| p.refinement && p.kind == FrameKind::State);
        ready &=
            (outstanding_bytes < window_bytes && oldest_us <= age_limit_us) || replace_refinement;
        let refine = ready && outstanding_bytes == 0;
        Budget {
            bitrate: self.bitrate,
            outstanding_bytes,
            window_bytes,
            oldest_us,
            ready,
            refine,
            decode_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_desktop_proto::FrameId;

    use super::*;
    fn feedback(ts: u64) -> Feedback {
        Feedback {
            received: Some(FrameId {
                epoch: 1,
                timestamp_us: ts,
            }),
            decoded: Some(FrameId {
                epoch: 1,
                timestamp_us: ts,
            }),
            presented: Some(FrameId {
                epoch: 1,
                timestamp_us: ts,
            }),
            delivery_bps: 2_000_000,
            rtt_us: 200_000,
            ..Default::default()
        }
    }

    #[test]
    fn budget_is_bytes_not_stop_and_wait_and_oversize_waits_for_receipt() {
        let now = Instant::now();
        let mut s = Sender::default();
        s.feedback(1, feedback(1), now);
        assert_eq!(s.budget(now).window_bytes, 75_000);
        s.encoded(2, 30_000, FrameKind::State, false, now);
        assert!(s.budget(now).ready);
        assert!(!s.budget(now).refine);
        s.encoded(3, 400_000, FrameKind::State, false, now);
        assert!(!s.budget(now).ready);
        s.feedback(1, feedback(2), now);
        assert!(
            !s.budget(now).ready,
            "partial progress cannot release the large frame"
        );
        s.feedback(1, feedback(3), now);
        assert!(s.budget(now).refine);
    }

    #[test]
    fn slow_viewer_disconnect_and_stale_feedback_have_different_effects() {
        let now = Instant::now();
        let mut s = Sender::default();
        s.feedback(1, feedback(1), now);
        s.feedback(2, feedback(1), now);
        s.encoded(2, 100_000, FrameKind::State, false, now);
        s.feedback(1, feedback(2), now);
        assert!(!s.budget(now).ready);
        s.remove(2);
        assert!(s.budget(now).ready);
        assert!(!s.budget(now + Duration::from_millis(1001)).ready);
        s.feedback(1, feedback(2), now + Duration::from_millis(1002));
        assert!(s.budget(now + Duration::from_millis(1002)).ready);
    }

    #[test]
    fn motion_can_replace_one_refinement_but_not_a_checkpoint_or_other_motion() {
        let now = Instant::now();
        for kind in [FrameKind::State, FrameKind::Checkpoint] {
            let mut s = Sender::default();
            s.feedback(1, feedback(1), now);
            s.encoded(2, 400_000, kind, true, now);
            assert_eq!(s.budget(now).ready, kind == FrameKind::State);
            assert!(!s.budget(now).refine);
            if kind == FrameKind::State {
                s.encoded(3, 90_000, FrameKind::State, false, now);
                assert!(
                    !s.budget(now).ready,
                    "replacement debt bounds cancellation bursts"
                );
                assert_eq!(s.budget(now).outstanding_bytes, 490_000);
            }
        }
    }

    #[test]
    fn slow_refinement_does_not_cut_the_motion_target() {
        let now = Instant::now();
        for refinement in [false, true] {
            let mut s = Sender::default();
            s.feedback(1, feedback(1), now);
            s.encoded(2, 20_000, FrameKind::State, false, now);
            s.encoded(3, 400_000, FrameKind::State, refinement, now);
            s.feedback(1, feedback(2), now + Duration::from_secs(1));
            assert_eq!(
                s.budget(now + Duration::from_secs(1)).bitrate,
                if refinement { 2_000_000 } else { 1_500_000 }
            );
        }
    }

    #[test]
    fn queue_inflated_rtt_does_not_expand_the_production_window() {
        let now = Instant::now();
        let mut s = Sender::default();
        let mut f = feedback(1);
        s.feedback(1, f, now);
        f.rtt_us = 2_000_000;
        s.feedback(1, f, now);
        assert_eq!(s.budget(now).window_bytes, 75_000);
    }

    #[test]
    fn tiny_frames_cannot_hide_an_old_queue_and_high_rtt_is_not_stop_and_wait() {
        let now = Instant::now();
        let mut s = Sender::default();
        let mut f = feedback(1);
        f.rtt_us = 600_000;
        s.feedback(1, f, now);
        assert_eq!(s.budget(now).window_bytes, 175_000);
        s.encoded(2, 200, FrameKind::State, false, now);
        s.feedback(1, f, now + Duration::from_millis(800));
        assert!(s.budget(now + Duration::from_millis(800)).ready);
        s.feedback(1, f, now + Duration::from_millis(801));
        assert!(
            !s.budget(now + Duration::from_millis(801)).ready,
            "fresh reports without receipt must not permit an ever-growing tiny-frame chain"
        );
        s.feedback(1, feedback(2), now + Duration::from_millis(802));
        assert!(s.budget(now + Duration::from_millis(802)).ready);
    }

    #[test]
    fn recovery_reports_are_idempotent_and_decoder_backlog_blocks_production() {
        let now = Instant::now();
        let mut s = Sender::default();
        let mut f = feedback(500_000);
        f.recover = true;
        f.recovery_id = 7;
        assert!(s.feedback(1, f, now));
        assert!(!s.feedback(1, f, now));
        f.recovery_id = 8;
        assert!(s.feedback(1, f, now));
        f.presented.as_mut().unwrap().timestamp_us = 200_000;
        s.feedback(1, f, now);
        assert!(!s.budget(now).ready);
        f.presented = f.received;
        s.feedback(1, f, now);
        assert!(s.budget(now).ready);
    }
}
