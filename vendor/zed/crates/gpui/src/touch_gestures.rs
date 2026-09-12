use std::{collections::HashMap, time::Duration};

use crate::GestureTuning;

const VELOCITY_WINDOW: Duration = Duration::from_millis(100);
const MOMENTUM_INTERVAL: Duration = Duration::from_millis(16);
const MOMENTUM_STOP_VELOCITY: f32 = 10.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Position {
    pub x: f32,
    pub y: f32,
}

impl Position {
    fn distance(self, other: Self) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }

    fn subtract(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
        }
    }

    fn scale(self, factor: f32) -> Self {
        Self {
            x: self.x * factor,
            y: self.y * factor,
        }
    }

    fn magnitude(self) -> f32 {
        (self.x.powi(2) + self.y.powi(2)).sqrt()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GestureAction {
    Click {
        position: Position,
        button: Button,
    },
    Scroll {
        position: Position,
        delta: Position,
        phase: Phase,
    },
    Pinch {
        position: Position,
        delta: f32,
        phase: Phase,
    },
}

#[derive(Clone, Copy, Debug)]
struct Contact {
    start: Position,
    position: Position,
    down_at: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    position: Position,
    at: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Momentum {
    position: Position,
    velocity: Position,
    last_at: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GestureState {
    #[default]
    PossibleTap,
    Panning,
    Pinching,
    LongPressed,
    Suppressed,
}

/// Pure touch gesture recognizer. All time values share an arbitrary monotonic epoch.
pub(crate) struct TouchGestureRecognizer {
    tuning: GestureTuning,
    contacts: HashMap<u64, Contact>,
    primary: Option<u64>,
    state: GestureState,
    samples: Vec<Sample>,
    pinch_distance: f32,
    momentum: Option<Momentum>,
}

impl Default for TouchGestureRecognizer {
    fn default() -> Self {
        Self::new(GestureTuning::default())
    }
}

impl TouchGestureRecognizer {
    pub(crate) fn new(tuning: GestureTuning) -> Self {
        Self {
            tuning,
            contacts: HashMap::new(),
            primary: None,
            state: GestureState::PossibleTap,
            samples: Vec::new(),
            pinch_distance: 0.0,
            momentum: None,
        }
    }

    pub fn down(&mut self, id: u64, position: Position, at: Duration) -> Vec<GestureAction> {
        let mut actions = Vec::new();
        if let Some(momentum) = self.momentum.take() {
            actions.push(GestureAction::Scroll {
                position: momentum.position,
                delta: Position::default(),
                phase: Phase::Ended,
            });
        }

        let was_empty = self.contacts.is_empty();
        let previous_primary_position = self
            .primary
            .and_then(|primary| self.contacts.get(&primary))
            .map(|contact| contact.position);
        let previous_pinch_center =
            (self.state == GestureState::Pinching).then(|| self.pinch_geometry().0);
        self.contacts.insert(
            id,
            Contact {
                start: position,
                position,
                down_at: at,
            },
        );
        if was_empty {
            self.primary = Some(id);
            self.state = GestureState::PossibleTap;
            self.samples.clear();
            self.samples.push(Sample { position, at });
        } else if self.contacts.len() == 2
            && matches!(
                self.state,
                GestureState::PossibleTap | GestureState::Panning
            )
        {
            if self.state == GestureState::Panning {
                actions.push(GestureAction::Scroll {
                    position: previous_primary_position.unwrap_or(position),
                    delta: Position::default(),
                    phase: Phase::Ended,
                });
            }
            self.state = GestureState::Pinching;
            let (center, distance) = self.pinch_geometry();
            self.pinch_distance = distance;
            actions.push(GestureAction::Pinch {
                position: center,
                delta: 0.0,
                phase: Phase::Started,
            });
        } else if self.contacts.len() > 2 {
            if self.state == GestureState::Pinching {
                actions.push(GestureAction::Pinch {
                    position: previous_pinch_center.unwrap_or_else(|| self.pinch_geometry().0),
                    delta: 0.0,
                    phase: Phase::Ended,
                });
            }
            self.state = GestureState::Suppressed;
        }
        actions
    }

    pub fn motion(&mut self, id: u64, position: Position, at: Duration) -> Vec<GestureAction> {
        let (previous, start) = {
            let Some(contact) = self.contacts.get_mut(&id) else {
                return Vec::new();
            };
            let previous = contact.position;
            contact.position = position;
            (previous, contact.start)
        };

        if self.state == GestureState::Pinching && self.contacts.len() == 2 {
            let (center, distance) = self.pinch_geometry();
            let delta = if self.pinch_distance > 0.0 {
                distance / self.pinch_distance - 1.0
            } else {
                0.0
            };
            self.pinch_distance = distance;
            return vec![GestureAction::Pinch {
                position: center,
                delta,
                phase: Phase::Moved,
            }];
        }

        if Some(id) != self.primary {
            return Vec::new();
        }

        self.record_sample(position, at);
        if self.state == GestureState::PossibleTap
            && position.distance(start) > self.tuning.touch_slop.as_f32()
        {
            self.state = GestureState::Panning;
            return vec![GestureAction::Scroll {
                position,
                delta: position.subtract(previous),
                phase: Phase::Started,
            }];
        }
        if self.state == GestureState::Panning {
            return vec![GestureAction::Scroll {
                position,
                delta: position.subtract(previous),
                phase: Phase::Moved,
            }];
        }
        Vec::new()
    }

    pub fn up(&mut self, id: u64, at: Duration) -> Vec<GestureAction> {
        let Some(contact) = self.contacts.remove(&id) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        match self.state {
            GestureState::PossibleTap if Some(id) == self.primary => {
                let elapsed = at.saturating_sub(contact.down_at);
                if elapsed >= self.tuning.long_press_duration {
                    actions.push(GestureAction::Click {
                        position: contact.position,
                        button: Button::Secondary,
                    });
                } else if elapsed <= self.tuning.tap_duration {
                    actions.push(GestureAction::Click {
                        position: contact.position,
                        button: Button::Primary,
                    });
                }
            }
            GestureState::Panning if Some(id) == self.primary => {
                self.record_sample(contact.position, at);
                let velocity = self.velocity();
                if velocity.magnitude() >= self.tuning.min_fling_velocity {
                    self.momentum = Some(Momentum {
                        position: contact.position,
                        velocity,
                        last_at: at,
                    });
                } else {
                    actions.push(GestureAction::Scroll {
                        position: contact.position,
                        delta: Position::default(),
                        phase: Phase::Ended,
                    });
                }
            }
            GestureState::Pinching => {
                let (center, _) = self.pinch_geometry();
                actions.push(GestureAction::Pinch {
                    position: if self.contacts.is_empty() {
                        contact.position
                    } else {
                        center
                    },
                    delta: 0.0,
                    phase: Phase::Ended,
                });
                self.state = GestureState::Suppressed;
            }
            _ => {}
        }

        if Some(id) == self.primary {
            self.primary = None;
        }
        if self.contacts.is_empty() {
            self.state = GestureState::PossibleTap;
        }
        actions
    }

    pub fn advance(&mut self, at: Duration) -> Vec<GestureAction> {
        if let Some(momentum) = self.momentum.as_mut() {
            let elapsed = at.saturating_sub(momentum.last_at);
            if elapsed.is_zero() {
                return Vec::new();
            }
            let elapsed_ms = elapsed.as_secs_f32() * 1000.0;
            let decay = self.tuning.momentum_decay_per_ms.powf(elapsed_ms);
            let delta = momentum.velocity.scale(elapsed.as_secs_f32());
            momentum.velocity = momentum.velocity.scale(decay);
            momentum.position = Position {
                x: momentum.position.x + delta.x,
                y: momentum.position.y + delta.y,
            };
            momentum.last_at = at;
            if momentum.velocity.magnitude() < MOMENTUM_STOP_VELOCITY {
                let position = momentum.position;
                self.momentum = None;
                return vec![GestureAction::Scroll {
                    position,
                    delta: Position::default(),
                    phase: Phase::Ended,
                }];
            }
            return vec![GestureAction::Scroll {
                position: momentum.position,
                delta,
                phase: Phase::Moved,
            }];
        }

        if self.state == GestureState::PossibleTap
            && let Some(primary) = self.primary
            && let Some(contact) = self.contacts.get(&primary)
            && at.saturating_sub(contact.down_at) >= self.tuning.long_press_duration
        {
            self.state = GestureState::LongPressed;
            return vec![GestureAction::Click {
                position: contact.position,
                button: Button::Secondary,
            }];
        }
        Vec::new()
    }

    pub fn cancel(&mut self) -> Vec<GestureAction> {
        let action = match self.state {
            GestureState::Panning => {
                self.primary
                    .and_then(|id| self.contacts.get(&id))
                    .map(|c| GestureAction::Scroll {
                        position: c.position,
                        delta: Position::default(),
                        phase: Phase::Cancelled,
                    })
            }
            GestureState::Pinching => Some(GestureAction::Pinch {
                position: self.pinch_geometry().0,
                delta: 0.0,
                phase: Phase::Cancelled,
            }),
            _ => self.momentum.map(|m| GestureAction::Scroll {
                position: m.position,
                delta: Position::default(),
                phase: Phase::Cancelled,
            }),
        };
        self.contacts.clear();
        self.primary = None;
        self.samples.clear();
        self.momentum = None;
        self.state = GestureState::PossibleTap;
        action.into_iter().collect()
    }

    pub fn has_momentum(&self) -> bool {
        self.momentum.is_some()
    }

    pub fn is_idle(&self) -> bool {
        self.contacts.is_empty()
    }

    pub fn long_press_duration(&self) -> Duration {
        self.tuning.long_press_duration
    }

    fn record_sample(&mut self, position: Position, at: Duration) {
        self.samples.push(Sample { position, at });
        let cutoff = at.saturating_sub(VELOCITY_WINDOW);
        self.samples.retain(|sample| sample.at >= cutoff);
    }

    fn velocity(&self) -> Position {
        let Some(first) = self.samples.first() else {
            return Position::default();
        };
        let Some(last) = self.samples.last() else {
            return Position::default();
        };
        let seconds = last.at.saturating_sub(first.at).as_secs_f32();
        if seconds <= 0.0 {
            Position::default()
        } else {
            last.position.subtract(first.position).scale(1.0 / seconds)
        }
    }

    fn pinch_geometry(&self) -> (Position, f32) {
        let mut contacts = self.contacts.values();
        let Some(first) = contacts.next() else {
            return (Position::default(), 0.0);
        };
        let Some(second) = contacts.next() else {
            return (first.position, 0.0);
        };
        (
            Position {
                x: (first.position.x + second.position.x) / 2.0,
                y: (first.position.y + second.position.y) / 2.0,
            },
            first.position.distance(second.position),
        )
    }
}

pub(crate) const fn momentum_interval() -> Duration {
    MOMENTUM_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    fn pos(x: f32, y: f32) -> Position {
        Position { x, y }
    }

    #[test]
    fn slop_boundary_remains_a_tap_but_exceeding_it_pans() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        assert!(gesture.motion(1, pos(8.0, 0.0), at(50)).is_empty());
        assert!(matches!(
            gesture.up(1, at(100)).as_slice(),
            [GestureAction::Click {
                button: Button::Primary,
                ..
            }]
        ));

        gesture.down(2, pos(0.0, 0.0), at(200));
        assert!(matches!(
            gesture.motion(2, pos(8.01, 0.0), at(250)).as_slice(),
            [GestureAction::Scroll {
                phase: Phase::Started,
                ..
            }]
        ));
    }

    #[test]
    fn long_press_wins_over_tap_at_deadline() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(4.0, 5.0), at(0));
        assert!(gesture.advance(at(499)).is_empty());
        assert!(matches!(
            gesture.advance(at(500)).as_slice(),
            [GestureAction::Click {
                button: Button::Secondary,
                ..
            }]
        ));
        assert!(gesture.up(1, at(510)).is_empty());
    }

    #[test]
    fn fling_uses_recent_motion_velocity() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        gesture.motion(1, pos(20.0, 0.0), at(100));
        gesture.motion(1, pos(50.0, 0.0), at(150));
        assert!(gesture.up(1, at(150)).is_empty());
        let actions = gesture.advance(at(166));
        let [
            GestureAction::Scroll {
                delta,
                phase: Phase::Moved,
                ..
            },
        ] = actions.as_slice()
        else {
            panic!("expected momentum scroll")
        };
        // The 100 ms velocity window retains the samples at 100 and 150 ms:
        // (50 - 20) / 0.05 s = 600 px/s, hence 9.6 px over a 16 ms tick.
        assert!((delta.x - 9.6).abs() < 0.01);
    }

    #[test]
    fn new_touch_ends_momentum() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        gesture.motion(1, pos(20.0, 0.0), at(50));
        gesture.up(1, at(50));
        assert!(gesture.has_momentum());
        assert_eq!(
            gesture.down(2, pos(2.0, 2.0), at(60)),
            vec![GestureAction::Scroll {
                position: pos(20.0, 0.0),
                delta: Position::default(),
                phase: Phase::Ended,
            }]
        );
        assert!(!gesture.has_momentum());
    }

    #[test]
    fn second_contact_ends_pan_at_the_primary_position() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        gesture.motion(1, pos(20.0, 0.0), at(50));
        let actions = gesture.down(2, pos(100.0, 100.0), at(60));
        assert!(matches!(
            actions.as_slice(),
            [
                GestureAction::Scroll {
                    position: Position { x: 20.0, y: 0.0 },
                    phase: Phase::Ended,
                    ..
                },
                GestureAction::Pinch {
                    phase: Phase::Started,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn two_contacts_produce_phased_pinch_updates() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        assert!(matches!(
            gesture.down(2, pos(10.0, 0.0), at(0)).as_slice(),
            [GestureAction::Pinch {
                phase: Phase::Started,
                ..
            }]
        ));
        let actions = gesture.motion(2, pos(12.0, 0.0), at(10));
        let [
            GestureAction::Pinch {
                position,
                delta,
                phase: Phase::Moved,
            },
        ] = actions.as_slice()
        else {
            panic!("expected pinch update")
        };
        assert_eq!(*position, pos(6.0, 0.0));
        assert!((*delta - 0.2).abs() < 0.001);
        assert!(matches!(
            gesture.up(2, at(20)).as_slice(),
            [GestureAction::Pinch {
                phase: Phase::Ended,
                ..
            }]
        ));
    }

    #[test]
    fn lifting_primary_during_pinch_suppresses_new_contacts_until_all_lift() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        gesture.down(2, pos(10.0, 0.0), at(0));
        gesture.up(1, at(10));
        assert!(gesture.down(3, pos(20.0, 0.0), at(20)).is_empty());
        assert!(gesture.up(3, at(30)).is_empty());
        assert!(gesture.up(2, at(40)).is_empty());
    }

    #[test]
    fn third_contact_ends_pinch_before_suppressing() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        gesture.down(2, pos(10.0, 0.0), at(0));
        assert_eq!(
            gesture.down(3, pos(20.0, 0.0), at(10)),
            vec![GestureAction::Pinch {
                position: pos(5.0, 0.0),
                delta: 0.0,
                phase: Phase::Ended,
            }]
        );
    }

    #[test]
    fn cancel_unwinds_pan_and_clears_contacts() {
        let mut gesture = TouchGestureRecognizer::default();
        gesture.down(1, pos(0.0, 0.0), at(0));
        gesture.motion(1, pos(20.0, 0.0), at(50));
        assert!(matches!(
            gesture.cancel().as_slice(),
            [GestureAction::Scroll {
                phase: Phase::Cancelled,
                ..
            }]
        ));
        assert!(gesture.up(1, at(60)).is_empty());
        assert!(!gesture.has_momentum());
    }
}
