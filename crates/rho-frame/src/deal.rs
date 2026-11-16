//! Dealing: what the dealer hands the user next, what the user says back,
//! and the lamp and chime that say something is waiting.
//!
//! The frame holds no queue. Every pull ranks the world as it stands
//! (`rho_dealer::rank`) over the claims every system makes, passes over
//! the card in view, and opens the top of the rest. A verdict is a fact
//! in the ledger, said once and synced; `shift-u` takes the last one
//! back, including what a system did about it outside the ledger.

use std::collections::BTreeSet;

use gpui::{Context, Window};
use jiff::{Timestamp, Zoned};
use rho_dealer::facts::Seen;
use rho_dealer::marks::NodeMarks;
use rho_dealer::rank::{Hand, Sources, Trace};
use rho_dealer::{Address, Card, Claim};
use rho_window::style::StyleClass;

use crate::attention::{Takeback, Undo, Verdict, Write};
use crate::frame::Frame;
use crate::home::{HOME_CAP, HomeRows};
use crate::when::{SnoozeUnit, snooze_target};

/// The lamp and the chime, as last evaluated.
#[derive(Default)]
pub(crate) struct Signals {
    pub lamp_on: bool,
    chime_above: bool,
    initialized: bool,
    scheduled: bool,
    pub next_change: Option<Timestamp>,
    /// The ranking as of the last change: what the status line, Home and
    /// a verdict read. Ranking asks every system for its claims and walks
    /// every host's chat, so it happens once per change, never per draw.
    pub hand: Hand,
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

    /// Ranks everything now. The result is kept as [`Signals::hand`] by
    /// whoever asked, so the next reader needs no rank of its own.
    fn rank(&self, cx: &gpui::App) -> Hand {
        let now = Zoned::now();
        self.with_sources(None, cx, |sources| rho_dealer::rank::rank(sources, &now))
    }

    /// The ranking as of the last change.
    pub fn hand(&self) -> &Hand {
        &self.signals.hand
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
            .hand()
            .cards
            .iter()
            .find(|card| &card.address == address)
        {
            return card.clone();
        }
        // A note is called by its first line.
        let (title, context) = match self.attention.marks.get(address).title() {
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

    /// Says again, of each node's new address, everything said of its old
    /// one, when `renamed` gives a node a new address that nothing was
    /// said of yet (`temp-migration`). The old address keeps its marks.
    pub fn restate(
        &mut self,
        renamed: &dyn Fn(&Address) -> Option<Address>,
        cx: &mut Context<Self>,
    ) {
        let marks = &self.attention.marks;
        let mut writes = Vec::new();
        for (node, held) in marks.nodes() {
            let Some(new) = renamed(node) else {
                continue;
            };
            if *marks.get(&new) != NodeMarks::default() {
                continue;
            }
            writes.extend(held.restated_as(&new).into_iter().map(Write::Fact));
        }
        self.write_marks(writes, cx);
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
        // A mute the system holds itself is only settled here: lifted
        // there, it is lifted.
        let ledger = match verdict {
            Verdict::Mute
                if self.systems.iter().any(|system| {
                    system.opens(address, cx) && system.mutes_itself(address, cx)
                }) =>
            {
                Verdict::Done
            }
            _ => verdict.clone(),
        };
        let fact = crate::attention::Attention::fact(address, &ledger, seen);
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
                .hand()
                .cards
                .iter()
                .find(|held| &held.address == address)
                .cloned()
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
        // A pull ranks afresh: the skip just said moves it on, and time
        // has passed since the last change.
        self.signals.hand = self.rank(cx);
        let Some(card) = self.hand().top(in_view.as_ref()).cloned() else {
            // Empty lands on Home: there is nothing to deal, so the glance
            // is the answer.
            self.open_home(window, cx);
            return;
        };
        self.journal_deal(Some(&card.address), cx);
        self.open_card(card, window, cx);
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

    /// Something a system did over many addresses at once, said as one
    /// thing `shift-u` takes back: the facts it writes and what the system
    /// did outside the ledger.
    pub fn say(
        &mut self,
        verb: String,
        writes: Vec<Write>,
        reversals: Vec<crate::system::Reversal>,
        cx: &mut Context<Self>,
    ) {
        let takeback = self.write_marks(writes, cx);
        self.attention.push_undo(Undo {
            sequence: 0,
            verb,
            takeback,
            card: None,
            reversals,
        });
        self.systems_changed(cx);
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
        let hand = self.rank(cx);
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
        self.signals.hand = hand;
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
        let mut sections = Vec::new();
        let mut channels = Vec::new();
        for system in &self.systems {
            sections.extend(system.home(&self.attention.marks, cx));
            channels.extend(system.channels(&self.attention.marks, cx));
        }
        let rows = HomeRows {
            next: hand.cards.iter().take(HOME_CAP).cloned().collect(),
            sections,
            channels,
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

pub(crate) fn journal_card(address: &Address) -> rho_journal::DealerCardIdentity {
    rho_journal::DealerCardIdentity {
        host: 0,
        node_id: address.clone().into(),
    }
}

fn journal_kind(address: &Address) -> rho_journal::DealerCardKind {
    match address.kind() {
        "thread" => rho_journal::DealerCardKind::Thread,
        "note" => rho_journal::DealerCardKind::Note,
        _ => rho_journal::DealerCardKind::Topic,
    }
}
