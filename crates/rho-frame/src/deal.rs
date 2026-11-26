//! Dealing: what the dealer hands the user next, what the user says back,
//! and the lamp and chime that say something is waiting.
//!
//! The frame holds no queue. Every pull ranks the world as it stands
//! (`rho_dealer::rank`) over the claims every system makes, passes over
//! the card in view, and opens the top of the rest. A verdict is a fact
//! in the ledger, said once and synced; `space u` takes the last one
//! back by retracting that fact.

use std::collections::BTreeSet;

use gpui::{Context, Window};
use jiff::{Timestamp, Zoned};
use rho_dealer::facts::Seen;
use rho_dealer::rank::{CardKind, Hand, Sources, Trace};
use rho_dealer::{Address, Card, Claim};
use rho_window::style::StyleClass;

use crate::attention::{Takeback, Undo, Verdict, Write};
use crate::desk::desk_rows;
use crate::frame::Frame;
use crate::phone::Layout;
use crate::when::{SnoozeUnit, snooze_said, snooze_target};

/// What the echo line says when a pull finds nothing to deal.
const NOTHING_WAITS: &str = "nothing waits";

/// The lamp and the chime, as last evaluated.
#[derive(Default)]
pub(crate) struct Signals {
    pub lamp_on: bool,
    /// The cards above the chime line as of the last evaluation, wherever
    /// the reader was; `None` before the first, which rings for nothing.
    /// The chime rings for a card that crosses the line, once.
    above_chime: Option<BTreeSet<Address>>,
    scheduled: bool,
    /// Ranks again when the hand next changes with nobody touching
    /// anything, such as a card's words saying another age.
    next_change: Option<gpui::Task<()>>,
    /// The ranking as of the last change: what the status line, Desk and
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
            .and_then(|system| system.name(address, &self.attention.marks, cx))
    }

    /// The screen that shows `address` as one of its lines, from the
    /// system that owns it.
    pub fn shown_in(&self, address: &Address, cx: &gpui::App) -> Option<Address> {
        self.systems
            .iter()
            .find(|system| system.opens(address, cx))
            .and_then(|system| system.shown_in(address, cx))
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
    pub(crate) fn journal_deal(&self, dealt: Option<&Address>, cx: &gpui::App) {
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
        let (title, context) = self
            .name_of(address, cx)
            .unwrap_or_else(|| (address.key().to_owned(), String::new()));
        Card {
            address: address.clone(),
            title,
            context,
            label: String::new(),
            kind: CardKind::Other,
            priority: f64::NEG_INFINITY,
            cursor: String::new(),
            seen: Seen::Whole,
            skipped: false,
        }
    }

    /// What the echo line calls a card: a note by its first line, anything
    /// else by the name its system gives it, and by the card's title
    /// when its system names nothing.
    fn called(&self, address: &Address, cx: &gpui::App) -> String {
        match self.attention.marks.get(address).title() {
            "" => self
                .name_of(address, cx)
                .map(|(name, _)| name)
                .unwrap_or_else(|| self.card_for(address, cx).title),
            title => title.to_owned(),
        }
    }

    /// What a verdict settles: what the reader has seen of the card, or
    /// the whole of a card that does not wait, such as a thread muted
    /// before it asks.
    fn seen(&self, address: &Address) -> Seen {
        self.card_seen(address).unwrap_or(Seen::Whole)
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
    /// the card closes and the next is dealt; a row on Desk stays where
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
        let called = self.called(address, cx);
        let seen = self.seen(address);
        let fact = crate::attention::Attention::fact(address, &verdict, seen);
        let takeback = self.write_marks(vec![Write::Fact(fact)], cx);
        let journal = verdict.journal();
        self.attention.push_undo(Undo {
            verb: verb.clone(),
            takeback,
            address: address.clone(),
            verdict: journal.clone(),
        });
        self.attention.skips.clear(address);
        rho_journal::record(rho_journal::Event::Dealer {
            card: journal_card(address),
            kind: journal_kind(address),
            verdict: journal,
            occurred_at: Timestamp::now().to_string(),
            skip_until: None,
        });
        let said = format!("{verb}: {called}");
        if matches!(verdict, Verdict::Unmute) || self.own_card(cx).as_ref() != Some(address) {
            self.echo(&said, StyleClass::SystemInfo, cx);
            self.systems_changed(cx);
            return;
        }
        self.close_current(window, cx);
        // The verdict and what comes of it, on one line: the last card put
        // away says the deal is empty too.
        match self.deal_next(None, window, cx) {
            true => self.echo(&said, StyleClass::SystemInfo, cx),
            false => {
                let words = if matches!(verdict, Verdict::Done)
                    && Layout::of(window.viewport_size().width) == Layout::Phone
                {
                    format!("{NOTHING_WAITS} · {said}")
                } else {
                    format!("{said} · {NOTHING_WAITS}")
                };
                self.echo(&words, StyleClass::SystemInfo, cx);
            }
        }
    }

    /// `space u`: the last verdict, taken back.
    pub fn undo_verdict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.attention.pop_undo() else {
            self.echo("nothing to undo", StyleClass::SystemInfo, cx);
            return;
        };
        let Undo {
            verb,
            takeback,
            address,
            verdict,
        } = undo;
        self.write_marks(takeback.into_iter().map(Takeback::write).collect(), cx);
        self.attention.skips.clear(&address);
        let called = self.called(&address, cx);
        rho_journal::record(rho_journal::Event::VerdictUndone {
            card: journal_card(&address),
            verdict,
        });
        self.echo(
            &format!("undid {verb}: {called}"),
            StyleClass::SystemInfo,
            cx,
        );
        if !self.current_surface().inspects_card(cx) {
            self.open(address, window, cx);
        }
        self.systems_changed(cx);
    }

    /// One pull: rank everything, pass over the card in view, and open
    /// the most important of the rest. Nothing is retained, so two pulls
    /// in a row see the same world and the skip is what moves them on.
    /// With nothing to deal, Desk opens and the echo line says so.
    pub fn pull_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deal_next(self.card_in_view(cx), window, cx) {
            self.echo(NOTHING_WAITS, StyleClass::SystemInfo, cx);
        }
    }

    /// On Desk, Space deals immediately if there is a card. If the deal
    /// is empty, it still gives the last verdict a route to Undo.
    pub(crate) fn desk_space(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.deal_next(self.card_in_view(cx), window, cx) {
            if self.attention.can_undo() {
                self.verdict_menu(window, cx);
            } else {
                self.echo(NOTHING_WAITS, StyleClass::SystemInfo, cx);
            }
        }
    }

    /// Whether a card opened. An explicit pull passes over its open card;
    /// a verdict already put that card away and passes over nothing else.
    /// Empty lands on Desk, since there is nothing to deal.
    fn deal_next(
        &mut self,
        in_view: Option<Address>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let now = Timestamp::now();
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
        let hand = self.rank(cx);
        let card = hand
            .top(in_view.as_ref())
            .or_else(|| hand.cards.first())
            .cloned();
        if card.is_none() {
            // Focus Desk against the empty deal, not the previous hand.
            self.refresh_desk(&hand, true, cx);
        }
        self.signals.hand = hand;
        let Some(card) = card else {
            if !self.desk_in_view() {
                self.open_desk(window, cx);
            }
            return false;
        };
        self.journal_deal(Some(&card.address), cx);
        self.open_card(card, window, cx);
        true
    }

    pub fn open_card(&mut self, card: Card, window: &mut Window, cx: &mut Context<Self>) {
        self.show_dealt_card(&card, window, cx);
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
        let now = Zoned::now();
        let (until, _) = snooze_target(unit, count.map_or(1, i64::from), &now);
        let said = snooze_said(&until.resolve(&now).to_zoned(now.time_zone().clone()), &now);
        self.verdict(address, Verdict::Snooze(until), said, window, cx);
    }

    /// Schedules one evaluation of the lamp, the chime and the desk for the
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
        self.refresh_desk(&hand, true, cx);
        self.signals.next_change = hand.next_change.map(|at| {
            // Ranked a moment ago, so a change already due is due now.
            let wait = std::time::Duration::try_from(Timestamp::now().duration_until(at))
                .unwrap_or_default();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |this, cx| this.systems_changed(cx));
            })
        });
        // The lamp and the chime signal a card only when it cannot already
        // be read here. Desk is itself a view of the hand, including its
        // overflow, so on Desk neither signals anything.
        let in_view = self.card_in_view(cx);
        let on_desk = self.desk_in_view();
        let elsewhere = || {
            hand.cards
                .iter()
                .filter(|card| !on_desk && Some(&card.address) != in_view.as_ref())
        };
        let top = elsewhere().max_by(|a, b| a.priority.total_cmp(&b.priority));
        let max_priority = top.map(|card| card.priority);
        let card = top.map(|card| journal_card(&card.address));
        let above_chime: BTreeSet<Address> = hand
            .cards
            .iter()
            .filter(|card| card.priority >= rho_dealer::curve::CHIME_THRESHOLD)
            .map(|card| card.address.clone())
            .collect();
        // Only a crossing rings. A card already above the line last time
        // rings no more, even when it crossed on Desk or in view, and a card
        // long above the line holds back no other card's crossing.
        let crossed = self.signals.above_chime.as_ref().and_then(|before| {
            elsewhere()
                .filter(|card| above_chime.contains(&card.address))
                .filter(|card| !before.contains(&card.address))
                .max_by(|a, b| a.priority.total_cmp(&b.priority))
                .map(|card| (card.priority, journal_card(&card.address)))
        });
        self.signals.above_chime = Some(above_chime);
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
        if let Some((priority, card)) = crossed {
            (self.chime)();
            rho_journal::record(rho_journal::Event::ChimeRing {
                top_priority: priority,
                card,
            });
        }
        cx.notify();
    }

    /// Desk is a window onto the same ranking, rebuilt wherever the dealer
    /// is invalidated or the hand changes by itself, never on a timer of
    /// its own.
    pub(crate) fn refresh_desk(&mut self, hand: &Hand, hold: bool, cx: &mut Context<Self>) {
        let rows = desk_rows(
            hand,
            |address| self.project_for(address, cx).map(|(_, project)| project),
            |address| {
                self.systems
                    .iter()
                    .find_map(|system| system.desk_context(address, cx))
            },
        );
        self.desk
            .update(cx, |desk, cx| desk.set_rows(rows, hold, cx));
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
        fyi_reply_pace_days: 0.0, // Results no longer fade.
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
