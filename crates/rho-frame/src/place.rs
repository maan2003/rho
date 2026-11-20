//! Where the user was on this device: the screen in view and the screens
//! back and forward from it, kept across a restart so the window opens
//! where they left it (`product.md`: where they were on each device).
//!
//! One row in the device's own database, rewritten whole after every move
//! on a thread of its own, so a move never waits on a commit. A screen is
//! kept by its address; the window makes it again only when the user
//! reaches it, and one that no longer opens leaves the history then.

use std::sync::mpsc;

use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, Sen, SenValue};
use rho_dealer::Address;
use rho_window::history::History;
use senax_encoder::{Decode, Encode};

const PLACE: TableDefinition<(), Sen<Saved>> = TableDefinition::new("gui_frame_place_v1");

/// The history as written down: every screen, oldest first, and which of
/// them is in view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct Saved {
    screens: Vec<String>,
    at: u32,
}

impl Saved {
    pub(crate) fn of(history: &History<Address, ()>) -> Self {
        let mut screens: Vec<String> = history
            .keys_back()
            .map(|address| address.to_string())
            .collect();
        screens.reverse();
        let at = screens.len() as u32;
        screens.push(history.current_key().to_string());
        screens.extend(history.keys_forward().map(|address| address.to_string()));
        Self { screens, at }
    }

    /// The history it describes, by address alone, with `home` in it
    /// whatever was written: Home is the screen nothing closes. An address
    /// this build cannot read is left out.
    pub(crate) fn history(&self, home: &Address) -> History<Address, ()> {
        let at = self
            .screens
            .get(self.at as usize)
            .and_then(|key| Address::parse(key));
        let mut screens: Vec<Address> = self
            .screens
            .iter()
            .filter_map(|key| Address::parse(key))
            .collect();
        if !screens.contains(home) {
            screens.insert(0, home.clone());
        }
        let mut history = History::new(screens[0].clone(), ());
        for address in &screens[1..] {
            history.show(address.clone(), ());
        }
        let at = at.unwrap_or_else(|| home.clone());
        while history.current_key() != &at && history.back().is_some() {}
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

/// Writes the newest place down, on a thread of its own: a run of moves
/// commits once, with the last of them.
pub(crate) struct Writer(mpsc::Sender<Saved>);

impl Writer {
    pub(crate) fn spawn(db: RhoDb) -> Self {
        let (sender, receiver) = mpsc::channel::<Saved>();
        std::thread::Builder::new()
            .name("rho-frame-place".to_owned())
            .spawn(move || {
                while let Ok(mut saved) = receiver.recv() {
                    while let Ok(newer) = receiver.try_recv() {
                        saved = newer;
                    }
                    futures::executor::block_on(async {
                        let mut write = db.write().await;
                        write
                            .open_table(PLACE)
                            .insert((), SenValue::borrowed(&saved));
                        write.commit();
                    });
                }
            })
            .expect("the place writer starts");
        Self(sender)
    }

    pub(crate) fn write(&self, saved: Saved) {
        // The thread ends only with the process.
        let _ = self.0.send(saved);
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
        let home = address("frame:home");
        let mut history = History::new(home.clone(), ());
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
        let mut restored = Saved::load(&db).unwrap().history(&home);
        assert_eq!(restored.current_key(), &address("agent:x"));
        assert_eq!(
            restored.keys_back().cloned().collect::<Vec<_>>(),
            [address("thread:a/1"), home.clone()]
        );
        assert_eq!(
            restored.keys_forward().cloned().collect::<Vec<_>>(),
            [address("thread:a/2")]
        );
        assert!(restored.back().is_some(), "ctrl-k is not at the oldest");
    }

    /// Home moved to the front of the stack stays where it was; one
    /// written without it, or with a screen this build cannot read, still
    /// has Home at the oldest end.
    #[test]
    fn home_is_always_in_the_history() {
        let home = address("frame:home");
        let saved = Saved {
            screens: vec![
                "thread:a/1".into(),
                "not an address".into(),
                "note:n".into(),
            ],
            at: 2,
        };
        let history = saved.history(&home);
        assert_eq!(history.current_key(), &address("note:n"));
        assert_eq!(
            history.keys_back().cloned().collect::<Vec<_>>(),
            [address("thread:a/1"), home.clone()]
        );

        let mut walked = History::new(home.clone(), ());
        walked.show(address("thread:a/1"), ());
        walked.show(home.clone(), ());
        let history = Saved::of(&walked).history(&home);
        assert_eq!(history.current_key(), &home);
        assert_eq!(
            history.keys_back().cloned().collect::<Vec<_>>(),
            [address("thread:a/1")]
        );
    }
}
