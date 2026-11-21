//! Where the user was on this device: the screen in view and the screens
//! back and forward from it, kept across a restart so the window opens
//! where they left it (`product.md`: where they were on each device).
//!
//! One row in the device's own database, rewritten whole after every move
//! on a thread of its own, so a move never waits on a commit. A screen is
//! kept by its address; the window makes it again only when the user
//! reaches it, and one that no longer opens leaves the history then.

use std::sync::mpsc;

use jiff::{SignedDuration, Timestamp};
use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, Sen, SenValue};
use rho_dealer::Address;
use rho_window::history::{History, Visit};
use senax_encoder::{Decode, Encode};

const PLACE: TableDefinition<(), Sen<Saved>> = TableDefinition::new("gui_frame_place_v1");
const LAST_INPUT: TableDefinition<(), i64> = TableDefinition::new("gui_frame_last_input_v1");
const IDEA_DRAFT_V1: TableDefinition<(), (&str, &str)> =
    TableDefinition::new("gui_frame_idea_draft_v1");
const IDEA_DRAFT: TableDefinition<(), Sen<Option<IdeaDraft>>> =
    TableDefinition::new("gui_frame_idea_draft_v2");

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct IdeaDraft {
    pub address: Option<String>,
    pub recipient: String,
    pub text: String,
}

/// `desk.a cold return opens the desk`: exactly ten minutes is still warm.
pub(crate) const COLD_RETURN: std::time::Duration = std::time::Duration::from_secs(600);

pub(crate) fn is_cold(last_input: Option<Timestamp>, now: Timestamp) -> bool {
    last_input.is_none_or(|last| {
        last.duration_until(now)
            > SignedDuration::try_from(COLD_RETURN).expect("cold return duration fits")
    })
}

pub(crate) fn last_input(db: &RhoDb) -> Option<Timestamp> {
    let read = db.read();
    if !read.has_table(LAST_INPUT.name()) {
        return None;
    }
    let millis = read.open_table(LAST_INPUT).get(())?.value();
    Timestamp::from_millisecond(millis).ok()
}

pub(crate) fn idea_draft(db: &RhoDb) -> Option<IdeaDraft> {
    let read = db.read();
    if read.has_table(IDEA_DRAFT.name()) {
        return read.open_table(IDEA_DRAFT).get(())?.value().into_owned();
    }
    // The previous editor could save only drafts bound to an address.
    if !read.has_table(IDEA_DRAFT_V1.name()) {
        return None;
    }
    let table = read.open_table(IDEA_DRAFT_V1);
    let value = table.get(())?;
    let (address, text) = value.value();
    Some(IdeaDraft {
        address: Some(address.to_owned()),
        recipient: String::new(),
        text: text.to_owned(),
    })
}

/// The history as written down: every screen, oldest first, and which of
/// them is in view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct Saved {
    screens: Vec<String>,
    at: u32,
    /// Same order as `screens`. Before the Palette recorded timestamps the
    /// stack was all that was saved; those screens have no known visit time.
    visits: Option<Vec<SavedVisit>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct SavedVisit {
    order: u64,
    wall_ms: Option<u64>,
}

impl Saved {
    pub(crate) fn of(history: &History<Address, ()>) -> Self {
        let mut keys: Vec<_> = history.keys_back().collect();
        keys.reverse();
        let at = keys.len() as u32;
        keys.push(history.current_key());
        keys.extend(history.keys_forward());
        let (screens, visits) = keys
            .into_iter()
            .map(|key| {
                let visit = history.visit(key).unwrap_or_default();
                (
                    key.to_string(),
                    SavedVisit {
                        order: visit.order,
                        wall_ms: visit.wall_ms,
                    },
                )
            })
            .unzip();
        Self {
            screens,
            at,
            visits: Some(visits),
        }
    }

    /// The history it describes, by address alone, with the desk in it
    /// whatever was written: the desk is the screen nothing closes. An address
    /// this build cannot read is left out.
    pub(crate) fn history(&self, desk: &Address) -> History<Address, ()> {
        // Reuse the same stack record; the oldest releases called the desk
        // Home. Translate that one surface instead of losing its position.
        let address = |key: &str| {
            if key == "frame:home" {
                Some(desk.clone())
            } else {
                Address::parse(key)
            }
        };
        let at = self
            .screens
            .get(self.at as usize)
            .and_then(|key| address(key));
        let mut screens: Vec<Address> =
            self.screens.iter().filter_map(|key| address(key)).collect();
        if !screens.contains(desk) {
            screens.insert(0, desk.clone());
        }
        let mut history = History::new(screens[0].clone(), ());
        for address in &screens[1..] {
            history.show(address.clone(), ());
        }
        let at = at.unwrap_or_else(|| desk.clone());
        while history.current_key() != &at && history.back().is_some() {}
        let visits = self
            .screens
            .iter()
            .enumerate()
            .filter_map(|(index, key)| {
                let key = address(key)?;
                let visit = self
                    .visits
                    .as_ref()
                    .and_then(|visits| visits.get(index))
                    .map_or_else(
                        || Visit {
                            order: history.visit(&key).map_or(0, |visit| visit.order),
                            wall_ms: None,
                        },
                        |saved| Visit {
                            order: saved.order,
                            wall_ms: saved.wall_ms,
                        },
                    );
                Some((key, visit))
            })
            .collect::<Vec<_>>();
        history.restore_visits(visits);
        history
    }

    /// What the last run wrote, if anything.
    pub(crate) fn load(db: &RhoDb) -> Option<Self> {
        let read = db.read();
        if !read.has_table(PLACE.name()) {
            return None;
        }
        let saved = read.open_table(PLACE).get(())?.value().into_owned();
        Some(saved)
    }
}

/// Starts on the desk after a cold return, with every other screen still
/// in the stack and the screen we left immediately behind the desk.
pub(crate) fn return_history(
    saved: &Saved,
    desk: &Address,
    last: Option<Timestamp>,
    now: Timestamp,
) -> History<Address, ()> {
    let mut history = saved.history(desk);
    if is_cold(last, now) {
        history.show(desk.clone(), ());
    }
    history
}

pub(crate) enum Write {
    Place(Saved),
    Input(Timestamp),
    IdeaDraft(Option<IdeaDraft>),
}

/// Saves the most recent update of each kind without waiting for disk in
/// a key handler. On a normal shutdown the writer finishes the queue.
pub(crate) struct Writer {
    sender: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    pub(crate) fn spawn(db: RhoDb) -> Self {
        let (sender, receiver) = mpsc::channel::<Write>();
        let writer = std::thread::Builder::new()
            .name("rho-frame-place".to_owned())
            .spawn(move || {
                while let Ok(first) = receiver.recv() {
                    let mut place = None;
                    let mut input = None;
                    let mut draft = None;
                    for write in std::iter::once(first).chain(receiver.try_iter()) {
                        match write {
                            Write::Place(value) => place = Some(value),
                            Write::Input(value) => input = Some(value),
                            Write::IdeaDraft(value) => draft = Some(value),
                        }
                    }
                    futures::executor::block_on(async {
                        let mut write = db.write().await;
                        if let Some(place) = place {
                            write
                                .open_table(PLACE)
                                .insert((), SenValue::borrowed(&place));
                        }
                        if let Some(input) = input {
                            write
                                .open_table(LAST_INPUT)
                                .insert((), input.as_millisecond());
                        }
                        if let Some(draft) = draft {
                            write
                                .open_table(IDEA_DRAFT)
                                .insert((), SenValue::borrowed(&draft));
                        }
                        write.commit();
                    })
                }
            })
            .expect("the place writer starts");
        Self {
            sender: Some(sender),
            writer: Some(writer),
        }
    }

    pub(crate) fn write(&self, write: Write) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(write);
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(key: &str) -> Address {
        Address::parse(key).unwrap()
    }

    /// The stack and the screen in view come back as they were, the way
    /// back and the way forward both, after a write and a read.
    #[test]
    fn the_history_comes_back_with_the_screen_in_view() {
        let desk = address("frame:desk");
        let mut history = History::new(desk.clone(), ());
        history.show(address("thread:a/1"), ());
        history.show(address("agent:x"), ());
        history.show(address("thread:a/2"), ());
        history.back();
        assert_eq!(history.current_key(), &address("agent:x"));

        let db = RhoDb::in_memory();
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write
                .open_table(PLACE)
                .insert((), SenValue::borrowed(&Saved::of(&history)));
            write.commit();
        });
        let mut restored = Saved::load(&db).unwrap().history(&desk);
        assert_eq!(restored.current_key(), &address("agent:x"));
        assert_eq!(
            restored.keys_back().cloned().collect::<Vec<_>>(),
            [address("thread:a/1"), desk.clone()]
        );
        assert_eq!(
            restored.keys_forward().cloned().collect::<Vec<_>>(),
            [address("thread:a/2")]
        );
        assert!(restored.back().is_some(), "ctrl-k is not at the oldest");
    }

    #[test]
    fn visited_times_survive_restart_and_legacy_history_does_not_invent_them() {
        #[derive(Encode, Decode)]
        struct Legacy {
            screens: Vec<String>,
            at: u32,
        }
        let mut bytes = bytes::BytesMut::new();
        use senax_encoder::{Decoder as _, Encoder as _};
        Legacy {
            screens: vec![
                "frame:desk".into(),
                "thread:old".into(),
                "thread:recent".into(),
            ],
            at: 1,
        }
        .encode(&mut bytes)
        .unwrap();
        let saved = Saved::decode(&mut bytes.clone().freeze()).unwrap();
        let desk = address("frame:desk");
        let old = address("thread:old");
        let recent = address("thread:recent");
        let mut history = saved.history(&desk);
        assert_eq!(history.current_key(), &old);
        assert_eq!(history.visit(&old).unwrap().wall_ms, None);
        assert_eq!(history.visit(&recent).unwrap().wall_ms, None);
        history.restore_visits([
            (
                desk.clone(),
                Visit {
                    order: 1,
                    wall_ms: Some(20),
                },
            ),
            (
                old.clone(),
                Visit {
                    order: 4,
                    wall_ms: Some(70),
                },
            ),
            (
                recent.clone(),
                Visit {
                    order: 3,
                    wall_ms: Some(70),
                },
            ),
        ]);
        let saved = Saved::of(&history);
        let mut bytes = bytes::BytesMut::new();
        saved.encode(&mut bytes).unwrap();
        let downgrade = Legacy::decode(&mut bytes.clone().freeze()).unwrap();
        assert_eq!(downgrade.at, 1);
        assert_eq!(
            downgrade.screens,
            ["frame:desk", "thread:old", "thread:recent"]
        );
        let restored = Saved::decode(&mut bytes.clone().freeze())
            .unwrap()
            .history(&desk);
        assert_eq!(restored.visit(&old), history.visit(&old));
        assert_eq!(restored.visit(&recent), history.visit(&recent));
        assert_eq!(restored.recent(), [&recent, &desk]);
    }

    /// The desk moved to the front of the stack stays where it was; one
    /// written without it, or with a screen this build cannot read, still
    /// has the desk at the oldest end.
    #[test]
    fn desk_is_always_in_the_history() {
        let desk = address("frame:desk");
        let saved = Saved {
            screens: vec![
                "thread:a/1".into(),
                "not an address".into(),
                "note:n".into(),
            ],
            at: 2,
            visits: None,
        };
        let history = saved.history(&desk);
        assert_eq!(history.current_key(), &address("note:n"));
        assert_eq!(
            history.keys_back().cloned().collect::<Vec<_>>(),
            [address("thread:a/1"), desk.clone()]
        );

        let mut walked = History::new(desk.clone(), ());
        walked.show(address("thread:a/1"), ());
        walked.show(desk.clone(), ());
        let history = Saved::of(&walked).history(&desk);
        assert_eq!(history.current_key(), &desk);
        assert_eq!(
            history.keys_back().cloned().collect::<Vec<_>>(),
            [address("thread:a/1")]
        );
    }

    #[test]
    fn a_cold_restart_keeps_the_stack_and_puts_the_screen_left_behind_the_desk() {
        let last: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let desk = address("frame:desk");
        let old = address("thread:old");
        let left = address("thread:left");
        let later = address("thread:later");
        let saved = Saved {
            screens: ["frame:home", "thread:old", "thread:left", "thread:later"]
                .map(str::to_owned)
                .to_vec(),
            at: 2,
            visits: None,
        };
        for warm in ["2026-11-20T12:09:59.999Z", "2026-11-20T12:10:00Z"] {
            let history = return_history(&saved, &desk, Some(last), warm.parse().unwrap());
            assert_eq!(
                history.current_key(),
                &left,
                "warm restart keeps the current screen"
            );
            assert_eq!(history.keys_forward().collect::<Vec<_>>(), [&later]);
            assert_eq!(
                history.keys_back().cloned().collect::<Vec<_>>(),
                [old.clone(), desk.clone()]
            );
        }
        let cold: Timestamp = "2026-11-20T12:10:00.001Z".parse().unwrap();
        for prior in [Some(last), None] {
            let mut history = return_history(&saved, &desk, prior, cold);
            assert_eq!(history.current_key(), &desk);
            assert_eq!(
                history.keys_back().cloned().collect::<Vec<_>>(),
                [left.clone(), later.clone(), old.clone()]
            );
            let reopened = return_history(&Saved::of(&history), &desk, Some(cold), cold);
            assert_eq!(
                reopened.current_key(),
                &desk,
                "a warm restart keeps the new place"
            );
            history.back();
            assert_eq!(
                history.current_key(),
                &left,
                "back returns to where the user left"
            );
        }
    }

    #[test]
    fn an_old_addressed_draft_is_kept_until_replaced_and_never_reappears_after_sending() {
        let db = RhoDb::in_memory();
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write
                .open_table(IDEA_DRAFT_V1)
                .insert((), ("thread:tide", "Check the tide"));
            write.commit();
        });
        assert_eq!(
            idea_draft(&db),
            Some(IdeaDraft {
                address: Some("thread:tide".into()),
                recipient: String::new(),
                text: "Check the tide".into(),
            })
        );
        let writer = Writer::spawn(db.clone());
        writer.write(Write::IdeaDraft(None));
        drop(writer);
        assert_eq!(idea_draft(&db), None);
        assert_eq!(
            db.read()
                .open_table(IDEA_DRAFT_V1)
                .get(())
                .unwrap()
                .value()
                .1,
            "Check the tide"
        );
    }

    #[test]
    fn input_and_the_unsent_idea_are_written_independently_of_navigation() {
        let db = RhoDb::in_memory();
        let at: Timestamp = "2026-11-20T12:09:59.999Z".parse().unwrap();
        let writer = Writer::spawn(db.clone());
        let draft = IdeaDraft {
            address: None,
            recipient: "tide@".into(),
            text: "Check the tide".into(),
        };
        writer.write(Write::IdeaDraft(Some(draft.clone())));
        writer.write(Write::Input(at));
        drop(writer);
        assert_eq!(last_input(&db), Some(at));
        assert_eq!(idea_draft(&db), Some(draft));
        assert!(Saved::load(&db).is_none(), "typing did not move the stack");
        let writer = Writer::spawn(db.clone());
        writer.write(Write::IdeaDraft(None));
        drop(writer);
        assert_eq!(idea_draft(&db), None);
        assert_eq!(
            last_input(&db),
            Some(at),
            "sending a draft is not a write of navigation"
        );
    }
}
