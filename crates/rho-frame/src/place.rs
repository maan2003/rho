//! Where the user was on this device: the places they visited, most
//! recent first, the one in view at the front, kept across a restart so
//! the window opens where they left it (`window.Screen`).
//!
//! One row in the device's own database, rewritten whole after every move
//! on a thread of its own, so a move never waits on a commit. A place is
//! kept by its address; the window makes its screen again only when the
//! user reaches it, and one that no longer opens leaves the order then.

use std::sync::mpsc;

use jiff::{SignedDuration, Timestamp};
use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, Sen, SenValue};
use rho_dealer::Address;
use senax_encoder::{Decode, Encode};

use crate::focus::DealerFocus;
use crate::frame::ScreenEntry;
use crate::recent::Recent;

const RECENT: TableDefinition<(), Sen<Saved>> = TableDefinition::new("gui_frame_recent_v1");
const LAST_INPUT: TableDefinition<(), i64> = TableDefinition::new("gui_frame_last_input_v1");
const COMMANDS: TableDefinition<(), Sen<SavedCommands>> =
    TableDefinition::new("gui_frame_recent_commands_v1");
const FOCUS: TableDefinition<(), Sen<DealerFocus>> =
    TableDefinition::new("gui_frame_dealer_focus_v1");

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

/// The places as written down, newest first: the first is in view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct Saved {
    places: Vec<SavedPlace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct SavedPlace {
    address: String,
    visited_ms: Option<u64>,
}

impl Saved {
    pub(crate) fn of(recent: &Recent<Address, ScreenEntry>) -> Self {
        Self {
            places: recent
                .record()
                .map(|(address, visited_ms)| SavedPlace {
                    address: address.to_string(),
                    visited_ms,
                })
                .collect(),
        }
    }

    /// The order it describes, by address alone, with the desk in it
    /// whatever was written: the desk is the place nothing closes. An
    /// address this build cannot read is left out.
    pub(crate) fn recent(&self, desk: &Address) -> Recent<Address, ScreenEntry> {
        let mut places: Vec<_> = self
            .places
            .iter()
            .filter_map(|place| {
                Some((
                    Address::parse(&place.address)?,
                    ScreenEntry::Opened,
                    place.visited_ms,
                ))
            })
            .collect();
        if !places.iter().any(|(address, ..)| address == desk) {
            places.push((desk.clone(), ScreenEntry::Opened, None));
        }
        Recent::restore(places).expect("the desk is always there")
    }

    /// What the last run wrote, if anything.
    pub(crate) fn load(db: &RhoDb) -> Option<Self> {
        let read = db.read();
        if read.has_table(RECENT.name()) {
            return Some(read.open_table(RECENT).get(())?.value().into_owned());
        }
        drop(read);
        legacy::load(db)
    }
}

/// Temporary: the linear back-and-forward history this record replaced,
/// read once so the first start of this build opens where the last one
/// left off. Remove once every device has written `gui_frame_recent_v1`.
mod legacy {
    use super::*;

    const PLACE: TableDefinition<(), Sen<History>> = TableDefinition::new("gui_frame_place_v1");

    #[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
    pub(super) struct History {
        pub screens: Vec<String>,
        pub at: u32,
        pub visits: Option<Vec<Visit>>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub(super) struct Visit {
        pub order: u64,
        pub wall_ms: Option<u64>,
    }

    pub(super) fn load(db: &RhoDb) -> Option<Saved> {
        let read = db.read();
        if !read.has_table(PLACE.name()) {
            return None;
        }
        let history = read.open_table(PLACE).get(())?.value().into_owned();
        Some(convert(&history))
    }

    /// The screen in view first, then the rest by their last visit; the
    /// oldest releases called the desk `home`.
    pub(super) fn convert(history: &History) -> Saved {
        let mut places: Vec<(usize, u64, SavedPlace)> = history
            .screens
            .iter()
            .enumerate()
            .map(|(index, screen)| {
                let visit = history.visits.as_ref().and_then(|visits| visits.get(index));
                let address = match screen.as_str() {
                    "frame:home" => "frame:desk".to_owned(),
                    other => other.to_owned(),
                };
                (
                    index,
                    visit.map_or(0, |visit| visit.order),
                    SavedPlace {
                        address,
                        visited_ms: visit.and_then(|visit| visit.wall_ms),
                    },
                )
            })
            .collect();
        let at = history.at as usize;
        places.sort_by_key(|(index, order, _)| {
            (
                *index != at,
                std::cmp::Reverse(*order),
                // Without visits, nearer the screen in view is more recent.
                index.abs_diff(at),
            )
        });
        Saved {
            places: places.into_iter().map(|(.., place)| place).collect(),
        }
    }
}

/// Starts on the desk after a cold return, with the place left right
/// behind it.
pub(crate) fn return_recent(
    saved: &Saved,
    desk: &Address,
    last: Option<Timestamp>,
    now: Timestamp,
) -> Recent<Address, ScreenEntry> {
    let mut recent = saved.recent(desk);
    if is_cold(last, now) {
        recent.show(desk.clone(), ScreenEntry::Opened);
    }
    recent
}

/// The commands the user ran, most recent first, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct SavedCommands {
    pub names: Vec<String>,
}

pub(crate) fn recent_commands(db: &RhoDb) -> Vec<String> {
    let read = db.read();
    if !read.has_table(COMMANDS.name()) {
        return Vec::new();
    }
    read.open_table(COMMANDS)
        .get(())
        .map(|saved| saved.value().into_owned().names)
        .unwrap_or_default()
}

/// The dealer focus this device was left with; all when none was set.
pub(crate) fn dealer_focus(db: &RhoDb) -> DealerFocus {
    let read = db.read();
    if !read.has_table(FOCUS.name()) {
        return DealerFocus::All;
    }
    read.open_table(FOCUS)
        .get(())
        .map(|saved| saved.value().into_owned())
        .unwrap_or_default()
}

pub(crate) enum Write {
    Place(Saved),
    Input(Timestamp),
    Commands(SavedCommands),
    Focus(DealerFocus),
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
                    let mut commands = None;
                    let mut focus = None;
                    for write in std::iter::once(first).chain(receiver.try_iter()) {
                        match write {
                            Write::Place(value) => place = Some(value),
                            Write::Input(value) => input = Some(value),
                            Write::Commands(value) => commands = Some(value),
                            Write::Focus(value) => focus = Some(value),
                        }
                    }
                    futures::executor::block_on(async {
                        let mut write = db.write().await;
                        if let Some(place) = place {
                            write
                                .open_table(RECENT)
                                .insert((), SenValue::borrowed(&place));
                        }
                        if let Some(input) = input {
                            write
                                .open_table(LAST_INPUT)
                                .insert((), input.as_millisecond());
                        }
                        if let Some(commands) = commands {
                            write
                                .open_table(COMMANDS)
                                .insert((), SenValue::borrowed(&commands));
                        }
                        if let Some(focus) = focus {
                            write
                                .open_table(FOCUS)
                                .insert((), SenValue::borrowed(&focus));
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

    fn order(recent: &Recent<Address, ScreenEntry>) -> Vec<Address> {
        recent.keys().cloned().collect()
    }

    /// The order and the place in view come back as they were, after a
    /// write and a read.
    #[test]
    fn the_order_comes_back_with_the_place_in_view() {
        let desk = address("frame:desk");
        let mut recent = Recent::new(desk.clone(), ScreenEntry::Opened);
        recent.show(address("thread:a/1"), ScreenEntry::Opened);
        recent.show(address("agent:x"), ScreenEntry::Opened);
        recent.show(address("thread:a/2"), ScreenEntry::Opened);
        recent.show(address("agent:x"), ScreenEntry::Opened);

        let db = RhoDb::in_memory();
        let writer = Writer::spawn(db.clone());
        writer.write(Write::Place(Saved::of(&recent)));
        drop(writer);
        let restored = Saved::load(&db).unwrap().recent(&desk);
        assert_eq!(order(&restored), order(&recent));
        assert_eq!(
            restored.visited_ms(&address("thread:a/1")),
            recent.visited_ms(&address("thread:a/1"))
        );
    }

    /// The history the last build wrote opens on its screen in view, with
    /// the rest by when they were visited, not by where they stood.
    #[test]
    fn the_last_builds_history_opens_in_visit_order() {
        let visit = |order, wall_ms| legacy::Visit { order, wall_ms };
        let history = legacy::History {
            screens: ["frame:home", "thread:old", "thread:left", "thread:later"]
                .map(str::to_owned)
                .to_vec(),
            at: 2,
            visits: Some(vec![
                visit(1, Some(10)),
                visit(4, Some(40)),
                visit(5, Some(50)),
                visit(3, None),
            ]),
        };
        let desk = address("frame:desk");
        let recent = legacy::convert(&history).recent(&desk);
        assert_eq!(
            order(&recent),
            [
                address("thread:left"),
                address("thread:old"),
                address("thread:later"),
                desk.clone(),
            ]
        );
        assert_eq!(recent.visited_ms(&address("thread:old")), Some(40));

        // An older record without visits: nearer the screen in view first.
        let bare = legacy::History {
            visits: None,
            ..history
        };
        assert_eq!(
            order(&legacy::convert(&bare).recent(&desk)),
            [
                address("thread:left"),
                address("thread:old"),
                address("thread:later"),
                desk.clone(),
            ]
        );
        let db = RhoDb::in_memory();
        futures::executor::block_on(async {
            let mut write = db.write().await;
            write
                .open_table(TableDefinition::<(), Sen<legacy::History>>::new(
                    "gui_frame_place_v1",
                ))
                .insert((), SenValue::borrowed(&bare));
            write.commit();
        });
        assert_eq!(Saved::load(&db), Some(legacy::convert(&bare)));
    }

    /// A record without the desk, or with an address this build cannot
    /// read, still has the desk, at the oldest end.
    #[test]
    fn desk_is_always_in_the_order() {
        let desk = address("frame:desk");
        let saved = Saved {
            places: ["note:n", "not an address", "thread:a/1"]
                .map(|place| SavedPlace {
                    address: place.to_owned(),
                    visited_ms: None,
                })
                .to_vec(),
        };
        assert_eq!(
            order(&saved.recent(&desk)),
            [address("note:n"), address("thread:a/1"), desk]
        );
    }

    #[test]
    fn a_cold_restart_opens_the_desk_with_the_place_left_right_behind_it() {
        let last: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let desk = address("frame:desk");
        let left = address("thread:left");
        let mut recent = Recent::new(desk.clone(), ScreenEntry::Opened);
        recent.show(address("thread:old"), ScreenEntry::Opened);
        recent.show(left.clone(), ScreenEntry::Opened);
        let saved = Saved::of(&recent);
        for warm in ["2026-11-20T12:09:59.999Z", "2026-11-20T12:10:00Z"] {
            let recent = return_recent(&saved, &desk, Some(last), warm.parse().unwrap());
            assert_eq!(recent.current_key(), &left, "warm restart keeps the place");
        }
        let cold: Timestamp = "2026-11-20T12:10:00.001Z".parse().unwrap();
        for prior in [Some(last), None] {
            let recent = return_recent(&saved, &desk, prior, cold);
            assert_eq!(recent.current_key(), &desk);
            assert_eq!(recent.previous().next(), Some(&left));
        }
    }

    #[test]
    fn input_and_commands_are_written_independently_of_navigation() {
        let db = RhoDb::in_memory();
        let at: Timestamp = "2026-11-20T12:09:59.999Z".parse().unwrap();
        let writer = Writer::spawn(db.clone());
        writer.write(Write::Input(at));
        writer.write(Write::Commands(SavedCommands {
            names: vec!["stop agent".into(), "quit".into()],
        }));
        drop(writer);
        assert_eq!(last_input(&db), Some(at));
        assert_eq!(recent_commands(&db), ["stop agent", "quit"]);
        assert!(Saved::load(&db).is_none(), "typing did not move the order");
    }

    /// The focus comes back after a restart as it was set last, a quiet
    /// with its end and a project by its address.
    #[test]
    fn the_dealer_focus_comes_back_as_it_was_last_set() {
        let db = RhoDb::in_memory();
        assert_eq!(dealer_focus(&db), DealerFocus::All, "nothing set yet");
        let until: Timestamp = "2026-11-26T12:45:00Z".parse().unwrap();
        let writer = Writer::spawn(db.clone());
        writer.write(Write::Focus(DealerFocus::QuietUntil(until)));
        drop(writer);
        assert_eq!(dealer_focus(&db), DealerFocus::QuietUntil(until));
        let tide = address("thread:lab/1");
        let writer = Writer::spawn(db.clone());
        writer.write(Write::Focus(DealerFocus::All));
        writer.write(Write::Focus(DealerFocus::Project(tide.clone())));
        drop(writer);
        assert_eq!(
            dealer_focus(&db),
            DealerFocus::Project(tide),
            "the last one said"
        );
    }
}
