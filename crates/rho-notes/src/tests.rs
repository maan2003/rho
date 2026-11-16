use jiff::Timestamp;
use jiff::tz::TimeZone;
use rho_dealer::facts::{Device, Entry};

use super::*;

const LAPTOP: Device = Device([1; 16]);

/// The user's marks, said one at a time a minute apart.
#[derive(Default)]
struct World {
    marks: Marks,
    minute: i64,
}

impl World {
    fn at(&mut self) -> jiff::Zoned {
        self.minute += 1;
        Timestamp::from_second(1_800_000_000 + self.minute * 60)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn say(&mut self, fact: Fact) {
        let at = self.at();
        self.marks.apply([Entry {
            device: LAPTOP,
            at,
            fact,
        }]);
    }

    fn note(&mut self, body: &str) -> NoteId {
        let note = NoteId(uuid::Uuid::new_v4());
        self.write(note, body, false);
        note
    }

    fn write(&mut self, note: NoteId, body: &str, deleted: bool) {
        let created = self.marks.get(&note.address()).created_ms.map_or_else(
            || self.at().timestamp(),
            |ms| Timestamp::from_millisecond(ms).unwrap(),
        );
        self.say(Fact::Wrote {
            note: note.0,
            text: body.to_owned(),
            created,
        });
        if deleted {
            self.say(Fact::Deleted { note: note.0 });
        }
    }
}

fn thread(n: u32) -> Address {
    Address::new("thread", format!("00000000000000ff/{n}"))
}

#[test]
fn a_note_has_one_address() {
    let id = uuid::Uuid::new_v4();
    let note = NoteId(id);
    assert_eq!(note.address(), Address::note(id));
    assert_eq!(NoteId::from_address(&Address::note(id)), Some(note));
    // The same id written another way is not the note's address.
    let hyphenated = Address::new(Address::NOTE, id.hyphenated());
    assert_eq!(NoteId::from_address(&hyphenated), None);
    assert_eq!(
        NoteId::from_address(&Address::new(
            Address::NOTE,
            id.simple().to_string().to_uppercase()
        )),
        None
    );
    // A label an older build minted under the same id is no note.
    assert_eq!(
        NoteId::from_address(&Address::new("label", id.simple())),
        None
    );
    assert_eq!(NoteId::from_address(&thread(1)), None);
}

#[test]
fn only_a_text_that_moved_is_written() {
    let mut world = World::default();
    let note = world.note("milk\neggs");
    let marks = &world.marks;
    assert!(write_for(marks, note, "milk\neggs".into()).is_none());
    match write_for(marks, note, "milk\neggs\nbread".into()) {
        Some(Write::Note {
            note: id,
            body: Some(body),
            deleted: None,
        }) => {
            assert_eq!(id, note.0);
            assert_eq!(body, "milk\neggs\nbread");
        }
        other => panic!("not a revision: {other:?}"),
    }
    // A note nobody wrote yet, left empty, says nothing.
    let fresh = NoteId(uuid::Uuid::new_v4());
    assert!(write_for(marks, fresh, String::new()).is_none());
}

/// The notes about a thing are its private replies, oldest first; a
/// deleted one is not.
#[test]
fn notes_about_a_thing_are_its_private_replies() {
    let mut world = World::default();
    let first = world.note("about seven");
    let other = world.note("about eight");
    let later = world.note("about seven, later");
    for (note, thread) in [(first, 7), (other, 8), (later, 7)] {
        world.say(Fact::About {
            node: note.address(),
            about: Some(self::thread(thread)),
        });
    }
    let replies = |world: &World, n| -> Vec<Address> {
        world
            .marks
            .replies_to(&thread(n))
            .into_iter()
            .map(|(address, _)| address.clone())
            .collect()
    };
    assert_eq!(replies(&world, 7), [first.address(), later.address()]);
    assert_eq!(replies(&world, 9), []);
    world.write(first, "about seven", true);
    assert_eq!(replies(&world, 7), [later.address()]);
}

/// A reply says what it is about and nothing else; a new note says
/// nothing at all, so once typed into it is a private room.
#[test]
fn a_new_note_is_about_where_it_was_made_or_nothing() {
    let note = NoteId(uuid::Uuid::new_v4());
    assert!(new_note_marks(note, None).is_empty());
    match new_note_marks(note, Some(&thread(7))).as_slice() {
        [Write::Fact(fact)] => assert_eq!(
            *fact,
            Fact::About {
                node: note.address(),
                about: Some(thread(7)),
            }
        ),
        other => panic!("not one subject: {other:?}"),
    }
}

#[test]
fn find_lists_live_notes() {
    let mut world = World::default();
    let kept = world.note("kept");
    let gone = world.note("gone");
    world.write(gone, "gone", true);
    let found: Vec<(String, String, Address)> = found(&world.marks)
        .into_iter()
        .map(|found| (found.title, found.description, found.address))
        .collect();
    assert_eq!(found, vec![("kept".into(), "note".into(), kept.address())]);
}

/// Home lists the notes about nothing as private rooms, the one last
/// written first; a reply under something is not a room.
#[test]
fn home_lists_the_private_rooms_newest_written_first() {
    let mut world = World::default();
    let older = world.note("older room");
    let reply = world.note("a reply");
    world.say(Fact::About {
        node: reply.address(),
        about: Some(thread(7)),
    });
    let newer = world.note("newer room");
    world.write(older, "older room, edited last", false);
    let rooms = private_rooms(&world.marks);
    let titles: Vec<&str> = rooms.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, ["older room, edited last", "newer room"]);
    assert_eq!(rooms[1].address, newer.address());
}
