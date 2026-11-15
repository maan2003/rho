//! Dealing: what the dealer hands the user next, what the user says back,
//! and the lamp and chime that say something is waiting.
//!
//! The frame holds no queue. Every pull ranks the world as it stands
//! (`rho_dealer::rank`) over the claims every system makes, passes over
//! the card in view, and opens the top of the rest. A verdict is a fact
//! in the ledger, said once and synced; `shift-u` takes the last one
//! back, including what a system did about it outside the ledger.

use std::collections::{BTreeSet, HashMap};

use gpui::{Context, Window};
use jiff::{Timestamp, Zoned};
use rho_dealer::facts::{Fact, Seen};
use rho_dealer::rank::{Hand, Sources, Trace};
use rho_dealer::{Address, Card, Claim, Until};
use rho_window::style::StyleClass;

use crate::attention::{Takeback, Undo, Verdict, Write};
use crate::frame::Frame;
use crate::home::{HOME_CAP, HomeRows, PileRow};
use crate::when::{SnoozeUnit, snooze_target};

/// The lamp and the chime, as last evaluated.
#[derive(Default)]
pub(crate) struct Signals {
    pub lamp_on: bool,
    chime_above: bool,
    initialized: bool,
    scheduled: bool,
    pub next_change: Option<Timestamp>,
}

impl Frame {
    /// Everything every system says wants the user, as it stands.
    fn claims(&self, trace: Option<&mut Trace>, cx: &gpui::App) -> Vec<Claim> {
        let now = Timestamp::now();
        let mut claims = Vec::new();
        let mut trace = trace;
        for system in &self.systems {
            claims.extend(system.claims(&self.attention.marks, now, trace.as_deref_mut(), cx));
        }
        claims
    }

    /// What an address is called and where it is, from the system that
    /// owns it.
    pub fn name_of(&self, address: &Address, cx: &gpui::App) -> Option<(String, String)> {
        self.systems
            .iter()
            .find(|system| system.opens(address, cx))
            .and_then(|system| system.name(address, cx))
    }

    fn with_sources<R>(
        &self,
        trace: Option<&mut Trace>,
        cx: &gpui::App,
        read: impl FnOnce(&Sources<'_>) -> R,
    ) -> R {
        let claims = self.claims(trace, cx);
        let names = |address: &Address| self.name_of(address, cx);
        let sources = Sources {
            claims: &claims,
            marks: &self.attention.marks,
            skips: &self.attention.skips,
            names: &names,
        };
        read(&sources)
    }

    /// The ranking as it stands.
    pub fn hand(&self, cx: &gpui::App) -> Hand {
        let now = Zoned::now();
        self.with_sources(None, cx, |sources| rho_dealer::rank::rank(sources, &now))
    }

    /// The ranking with everything it weighed, into the journal.
    fn journal_deal(&self, dealt: Option<&Address>, cx: &gpui::App) {
        let now = Zoned::now();
        let mut trace = Trace::default();
        let (hand, trace) = self.with_sources(Some(&mut trace), cx, |sources| {
            rho_dealer::rank::rank_traced(sources, &now, Trace::default())
        });
        rho_journal::record(rho_journal::Event::Deal {
            trigger: rho_journal::DealTrigger::Pull,
            occurred_at: now.timestamp().to_string(),
            zone: now.time_zone().iana_name().unwrap_or("unnamed").to_owned(),
            dealt: dealt.map(journal_card),
            hand: hand
                .cards
                .iter()
                .map(|card| rho_journal::DealtCard {
                    card: journal_card(&card.address),
                    kind: journal_kind(&card.address),
                    priority: card.priority,
                    label: card.label.clone(),
                    title: card.title.clone(),
                    context: card.context.clone(),
                    cursor: card.cursor.clone(),
                    skipped: card.skipped,
                })
                .collect(),
            weighed: trace
                .nodes
                .into_iter()
                .map(|(address, weighed)| rho_journal::WeighedNode {
                    card: journal_card(&address),
                    outcome: weighed.outcome,
                    inputs: weighed
                        .inputs
                        .into_iter()
                        .map(|(key, value)| rho_journal::Input {
                            key: key.to_owned(),
                            value,
                        })
                        .collect(),
                    parts: weighed.parts,
                })
                .collect(),
            next_change: hand.next_change.map(|at| at.to_string()),
        });
    }

    /// The card for an address: its card in the hand, or the address
    /// itself when the hand holds none, because reading a thing can be
    /// what quiets it and a verdict on what is on screen still lands.
    pub fn card_for(&self, address: &Address, cx: &gpui::App) -> Card {
        if let Some(card) = self
            .hand(cx)
            .cards
            .into_iter()
            .find(|card| &card.address == address)
        {
            return card;
        }
        let marks = &self.attention.marks;
        // A label is called by its path; a note by its first line.
        let title = match address.as_label() {
            Some(label) => marks.label_path(label),
            None => marks.get(address).title().to_owned(),
        };
        let (title, context) = match title.as_str() {
            "" => self
                .name_of(address, cx)
                .unwrap_or_else(|| (address.key().to_owned(), String::new())),
            title => (title.to_owned(), String::new()),
        };
        Card {
            address: address.clone(),
            title,
            context,
            label: String::new(),
            priority: f64::NEG_INFINITY,
            cursor: String::new(),
            skipped: false,
        }
    }

    /// How far the user has seen `address`: everything its system has.
    fn seen(&self, address: &Address, cx: &gpui::App) -> Seen {
        self.systems
            .iter()
            .find(|system| system.opens(address, cx))
            .map_or(Seen::Whole, |system| system.seen(address, cx))
    }

    /// Says `writes`, and returns what takes them back.
    pub fn write_marks(&mut self, writes: Vec<Write>, cx: &mut Context<Self>) -> Vec<Takeback> {
        if writes.is_empty() {
            return Vec::new();
        }
        let (takeback, touched) = self.attention.say(writes);
        self.marks_moved(touched, cx);
        takeback
    }

    pub(crate) fn marks_moved(&mut self, touched: BTreeSet<Address>, cx: &mut Context<Self>) {
        if touched.is_empty() {
            return;
        }
        for system in self.systems.clone() {
            system.marks_moved(&touched, cx);
        }
        self.systems_changed(cx);
    }

    /// A verdict on `address`, said in `verb`'s words. The surface showing
    /// the card closes and the next is dealt; a row on Home stays where
    /// the reader is.
    pub fn verdict(
        &mut self,
        address: &Address,
        verdict: Verdict,
        verb: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!(address = %address, ?verdict, "verdict");
        let card = self.card_for(address, cx);
        let seen = self.seen(address, cx);
        let fact = crate::attention::Attention::fact(address, &verdict, seen);
        let takeback = self.write_marks(vec![Write::Fact(fact)], cx);
        let mut reversals = Vec::new();
        for system in self.systems.clone() {
            if system.opens(address, cx) {
                reversals.extend(system.said(address, &verdict, cx));
            }
        }
        let journal = verdict.journal();
        self.attention.push_undo(Undo {
            sequence: 0,
            verb: verb.clone(),
            takeback,
            card: Some((address.clone(), journal.clone())),
            reversals,
        });
        self.attention.skips.clear(address);
        rho_journal::record(rho_journal::Event::Dealer {
            card: journal_card(address),
            kind: journal_kind(address),
            verdict: journal,
            occurred_at: Timestamp::now().to_string(),
            skip_until: None,
        });
        self.echo(
            &format!("{verb}: {}", card.title),
            StyleClass::SystemInfo,
            cx,
        );
        if self.card_in_view(cx).as_ref() == Some(address) {
            self.close_current(window, cx);
            self.pull_card(window, cx);
        } else {
            self.systems_changed(cx);
        }
    }

    /// `shift-u`: the last verdict, taken back.
    pub fn undo_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.attention.pop_undo() else {
            self.echo("nothing to undo", StyleClass::SystemInfo, cx);
            return;
        };
        for reversal in undo.reversals {
            reversal(cx);
        }
        let writes = undo.takeback.into_iter().map(Takeback::write).collect();
        self.write_marks(writes, cx);
        let Some((address, verdict)) = undo.card else {
            self.echo(&format!("undid {}", undo.verb), StyleClass::SystemInfo, cx);
            self.systems_changed(cx);
            return;
        };
        self.attention.skips.clear(&address);
        let card = self.card_for(&address, cx);
        rho_journal::record(rho_journal::Event::VerdictUndone {
            card: journal_card(&address),
            verdict,
        });
        self.echo(
            &format!("undid {}: {}", undo.verb, card.title),
            StyleClass::SystemInfo,
            cx,
        );
        self.open(address, window, cx);
        self.systems_changed(cx);
    }

    /// One pull: rank everything, pass over the card in view, and open
    /// the most important of the rest. Nothing is retained, so two pulls
    /// in a row see the same world and the skip is what moves them on.
    pub fn pull_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Timestamp::now();
        let in_view = self.card_in_view(cx);
        // Reading a card and pulling again is what a skip is: the card is
        // still owed, it is just not what to look at next.
        if let Some(address) = &in_view
            && let Some(held) = self
                .hand(cx)
                .cards
                .into_iter()
                .find(|held| &held.address == address)
        {
            self.attention
                .skips
                .skip(held.address.clone(), held.cursor.clone(), now);
            rho_journal::record(rho_journal::Event::Dealer {
                card: journal_card(address),
                kind: journal_kind(address),
                verdict: rho_journal::DealerVerdict::Skip,
                occurred_at: now.to_string(),
                skip_until: Some((now + rho_dealer::curve::SKIP_HOLD).to_string()),
            });
        }
        if self.attention.open_pile.is_some() {
            self.pull_from_pile(in_view, window, cx);
            return;
        }
        let Some(card) = self.hand(cx).top(in_view.as_ref()).cloned() else {
            // Empty lands on Home: there is nothing to deal, so the glance
            // is the answer.
            self.open_home(window, cx);
            return;
        };
        self.journal_deal(Some(&card.address), cx);
        self.open_card(card, window, cx);
    }

    /// Deals from the open pile: the next card on it this sitting has not
    /// passed. Off the end, the pile closes and Home is the answer.
    fn pull_from_pile(
        &mut self,
        in_view: Option<Address>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let piles = self.attention.marks.piles(Timestamp::now());
        let Some(open) = self.attention.open_pile.as_mut() else {
            return;
        };
        open.passed.extend(in_view);
        let name = open.name.clone().unwrap_or_else(|| "later".to_owned());
        let next = piles
            .into_iter()
            .find(|pile| pile.name == open.name)
            .map(|pile| pile.nodes)
            .unwrap_or_default()
            .into_iter()
            .find(|address| !open.passed.contains(address));
        let Some(address) = next else {
            self.attention.open_pile = None;
            self.echo(&format!("{name}: nothing more"), StyleClass::SystemInfo, cx);
            self.open_home(window, cx);
            return;
        };
        let card = self.card_for(&address, cx);
        self.open_card(card, window, cx);
    }

    /// Enter on a pile row: the pile is dealt from until it runs out.
    pub fn open_pile(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.attention.open_pile = Some(crate::attention::OpenPile {
            name,
            passed: Default::default(),
        });
        self.pull_from_pile(None, window, cx);
    }

    fn open_card(&mut self, card: Card, window: &mut Window, cx: &mut Context<Self>) {
        self.open_with(
            card.address,
            rho_journal::SurfaceShowMethod::Deal,
            window,
            cx,
        );
        self.systems_changed(cx);
    }

    /// A snooze from the verdict menu: `count` of `unit`, or one.
    pub fn snooze(
        &mut self,
        address: &Address,
        unit: SnoozeUnit,
        count: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (until, said) = snooze_target(unit, count.map_or(1, i64::from), &Zoned::now());
        self.verdict(address, Verdict::Snooze(until), said, window, cx);
    }

    /// A todo from the verdict menu: on the plate now, or from `count`
    /// days on.
    pub fn todo(
        &mut self,
        address: &Address,
        count: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = Zoned::now();
        let (start, said) = match count {
            None => (None, "todo".to_owned()),
            Some(days) => {
                let date = now
                    .date()
                    .checked_add(jiff::Span::new().days(i64::from(days)))
                    .unwrap_or(now.date());
                (
                    Some(Until::Day(date)),
                    format!("todo from {}", date.strftime("%a %-d %b")),
                )
            }
        };
        self.verdict(address, Verdict::Todo { start }, said, window, cx);
    }

    /// The label at `path`, made if nobody has made it yet, with every
    /// label above it.
    pub fn mint_label(&mut self, path: &str, cx: &mut Context<Self>) -> Option<uuid::Uuid> {
        let names = crate::attention::path_names(path);
        if names.is_empty() {
            return None;
        }
        let mut parent: Option<uuid::Uuid> = None;
        let mut writes = Vec::new();
        let mut minted: HashMap<String, uuid::Uuid> = HashMap::new();
        for depth in 0..names.len() {
            let prefix = names[..=depth].join("/");
            let found = self
                .attention
                .marks
                .label_at(&prefix)
                .or_else(|| minted.get(&prefix).copied());
            let id = match found {
                Some(id) => id,
                None => {
                    let id = uuid::Uuid::new_v4();
                    writes.push(Write::Fact(Fact::Label {
                        label: id,
                        name: names[depth].to_owned(),
                        parent,
                    }));
                    minted.insert(prefix, id);
                    id
                }
            };
            parent = Some(id);
        }
        self.write_marks(writes, cx);
        parent
    }

    /// Puts the label at `path` on `address`, or takes it off when it is
    /// already there. `shift-u` takes it back.
    pub fn toggle_label(&mut self, address: &Address, path: &str, cx: &mut Context<Self>) {
        let path = path.trim();
        let existing = self.attention.marks.label_at(path);
        let carried =
            existing.is_some_and(|label| self.attention.marks.get(address).labels.contains(&label));
        let label = match existing {
            Some(label) => label,
            None => match self.mint_label(path, cx) {
                Some(label) => label,
                None => {
                    self.echo("file: no label", StyleClass::SystemInfo, cx);
                    return;
                }
            },
        };
        let takeback = self.write_marks(
            vec![Write::Fact(Fact::Labeled {
                node: address.clone(),
                label,
                present: !carried,
            })],
            cx,
        );
        let title = self.card_for(address, cx).title;
        let said = match carried {
            true => format!("unfiled {title} from {path}"),
            false => format!("filed {title} under {path}"),
        };
        self.attention.push_undo(Undo {
            sequence: 0,
            verb: said.clone(),
            takeback,
            card: None,
            reversals: Vec::new(),
        });
        self.echo(&said, StyleClass::SystemInfo, cx);
    }

    /// Schedules one evaluation of the lamp, the chime and Home for the
    /// end of this effect cycle, however many changes asked.
    pub fn systems_changed(&mut self, cx: &mut Context<Self>) {
        if self.signals.scheduled {
            return;
        }
        self.signals.scheduled = true;
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                this.signals.scheduled = false;
                this.evaluate_signals(cx);
            });
        })
        .detach();
    }

    fn evaluate_signals(&mut self, cx: &mut Context<Self>) {
        let hand = self.hand(cx);
        self.signals.next_change = hand.next_change;
        self.refresh_home(&hand, cx);
        // What the lamp is about is what is *not* in front of the reader:
        // the card they are already reading is not news.
        let in_view = self.card_in_view(cx);
        let top = hand
            .cards
            .iter()
            .find(|card| Some(&card.address) != in_view.as_ref());
        let max_priority = top.map(|card| card.priority);
        let card = top.map(|card| journal_card(&card.address));
        let lamp_on =
            max_priority.is_some_and(|priority| priority >= rho_dealer::curve::LAMP_THRESHOLD);
        if lamp_on != self.signals.lamp_on {
            self.signals.lamp_on = lamp_on;
            rho_journal::record(rho_journal::Event::LampTransition {
                state: match lamp_on {
                    true => rho_journal::SignalState::On,
                    false => rho_journal::SignalState::Off,
                },
                top_priority: max_priority,
                card: card.clone(),
            });
        }
        let chime_above =
            max_priority.is_some_and(|priority| priority >= rho_dealer::curve::CHIME_THRESHOLD);
        if !self.signals.initialized {
            self.signals.initialized = true;
            self.signals.chime_above = chime_above;
            cx.notify();
            return;
        }
        if chime_above
            && !self.signals.chime_above
            && let (Some(priority), Some(card)) = (max_priority, card)
        {
            (self.chime)();
            rho_journal::record(rho_journal::Event::ChimeRing {
                top_priority: priority,
                card,
            });
        }
        self.signals.chime_above = chime_above;
        cx.notify();
    }

    /// Home is a window onto the same ranking, rebuilt wherever the dealer
    /// is invalidated and never on a timer.
    fn refresh_home(&mut self, hand: &Hand, cx: &mut Context<Self>) {
        let now = Timestamp::now();
        let mut sections = Vec::new();
        let mut channels = Vec::new();
        for system in &self.systems {
            sections.extend(system.home(&self.attention.marks, cx));
            channels.extend(system.channels(&self.attention.marks, cx));
        }
        let piles = self
            .attention
            .marks
            .piles(now)
            .into_iter()
            .map(|pile| PileRow {
                name: pile.name,
                count: pile.nodes.len(),
                back: pile
                    .back
                    .map(|back| back_words(back, now))
                    .unwrap_or_default(),
            })
            .collect();
        let rows = HomeRows {
            next: hand.cards.iter().take(HOME_CAP).cloned().collect(),
            sections,
            channels,
            piles,
        };
        self.home.update(cx, |home, cx| home.set_rows(rows, cx));
    }
}

/// The dealer's policy constants as the journal records them at session
/// start.
pub fn dealer_policy_snapshot() -> rho_journal::DealerPolicySnapshot {
    use rho_dealer::curve::*;
    rho_journal::DealerPolicySnapshot {
        queue_floor: DEAL_QUEUE_FLOOR,
        skip_cooldown_minutes: SKIP_HOLD.as_mins(),
        blocked_reply_head_start: AGENT_BLOCKED_HEAD_START,
        blocked_reply_slope_per_day: WAITING_SLOPE_PER_DAY,
        fyi_reply_pace_days: AGENT_FINISHED_GONE_DAYS,
        thread_reply_head_start: SLACK_THREAD_HEAD_START,
        channel_traffic_head_start: CHANNEL_TRAFFIC_HEAD_START,
        channel_answered_drop: 0.0,
        lamp_threshold: LAMP_THRESHOLD,
        chime_threshold: CHIME_THRESHOLD,
        agent_recency_bonus: AGENT_RECENCY_BONUS,
        agent_recency_window_ms: AGENT_RECENCY_WINDOW.as_millis() as i64,
    }
}

/// When a snoozed pile comes back, in the deal bar's units.
fn back_words(back: Timestamp, now: Timestamp) -> String {
    let left = back.duration_since(now);
    let minutes = left.as_mins();
    match minutes {
        ..=0 => "back now".to_owned(),
        1..60 => format!("back in {minutes}m"),
        60..1440 => format!("back in {}h", minutes / 60),
        _ => format!("back in {}d", minutes / 1440),
    }
}

pub(crate) fn journal_card(address: &Address) -> rho_journal::DealerCardIdentity {
    rho_journal::DealerCardIdentity {
        host: 0,
        node_id: address.clone().into(),
    }
}

fn journal_kind(address: &Address) -> rho_journal::DealerCardKind {
    match address.kind() {
        "thread" => rho_journal::DealerCardKind::Thread,
        "note" | "label" => rho_journal::DealerCardKind::Note,
        _ => rho_journal::DealerCardKind::Topic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pile_says_when_it_comes_back() {
        let now = Timestamp::UNIX_EPOCH;
        assert_eq!(back_words(now, now), "back now");
        assert_eq!(
            back_words(now + jiff::SignedDuration::from_mins(45), now),
            "back in 45m"
        );
        assert_eq!(
            back_words(now + jiff::SignedDuration::from_hours(5), now),
            "back in 5h"
        );
        assert_eq!(
            back_words(now + jiff::SignedDuration::from_hours(50), now),
            "back in 2d"
        );
    }
}
