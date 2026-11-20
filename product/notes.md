# notes

Notes are the user's memory: text only they write and only their devices hold. They live apart from the work and can be found from the user's palette. A note is about nothing and listed in `Notes`, or about a `mail.Mail` and also a private reply beside it.

## concept Note
status: the palette finds notes by title only; full-text search is on `Notes`.
purpose: keep something the user wants to remember, in their own words, where no agent can read it.
principle: the user thinks of something to check tomorrow; they run `new note` and type it; it is kept a moment after they stop typing; the next day they run `notes`, and it is at the top of the list.
sees: its first line, which is its title, or `untitled note`; open, its whole text, to edit like any text.
does:
- new note, from the palette: open a new, empty note ready to type in Insert; it reads `a note · its first line is its title` until the user types
- edit: type in it; what is typed is kept in under a second after the user stops, and on quit
- notes, from the palette: open `Notes`, which lists every note
- open `enter` on a note in `Notes`: open the note

No agent sees a note. A note's text passes between the user's devices through the hosts, sealed, the same way as verdicts: only between devices that hold the user's secret phrase; see `hosts.Pair this device`. A note left empty disappears from the list and the thread. While the note is still open, `u` restores the text and `shift-u` redoes the edit.

While the user types a new note, the top line follows its first line as it is typed. A note is saved the moment the user leaves it.

## concept Private reply
purpose: let the user keep a remark on a `mail.Mail` in its thread, where only they see it.
principle: a project mails a result the user doubts; they press `alt-r` on the mail, type their doubt just beneath it and press `escape`. The remark stays there, dim, marked `🔒 you`.
sees: directly under the mail, `🔒 you · time` and the text, dim and slanted; while writing, a line says `── private · only you see this · escape keeps it`.
does:
- reply privately `alt-r` on the mail, or `reply privately` from the palette or phone menu: start writing beneath it; if the mail already has a private reply, edit the latest one there
- edit `enter` or a tap on an existing private reply: write in place; `escape`, or `keep` on the phone, returns to reading

## concept Find or start
purpose: find a `Note` by words the user remembers, or start one with those words.
principle: the user types `signed copy` and sees the note whose second line says to bring it; the words only become a new note if nothing matches and the user presses Enter.
sees: `find or start:`, the words typed, and only the `Note` rows containing every word, in any order, anywhere in their text.
does:
- search `/` or tap: put the cursor in the line and type to narrow the list
- open `enter`: open the selected row; with no match, start a note with the typed text as its first line, in Insert
- leave `escape`: move to the rows without opening either a match or a new note

## screen Notes
answers: What did the user write down, and can they find it or start another note?
shows: `Find or start` at the top; then `Note` rows, last written first, with their first line, a few quieter words from the rest, and age; a `Private reply` adds its mail's first words after `↳`, and the thread's subject when different.
states:
- empty: `no notes yet · i or o to start` (on the phone, `no notes yet · tap new to start`)
- loading: no separate loading state; the device lists the notes it holds
- partial: only matching rows; with none, `no match · enter to start a note`
- error: no separate error state
- full: all nonempty notes, private replies too; on return from a note, the list picks that note if it is still there and shows all rows again
keys:
- `enter` or tap a row: open the picked note
- `/` or tap `find or start:`: find by any words in a note
- `down` and `up` in the query: pick a match
- `o`, `i` with no notes, or `new` on the phone: start an empty note in Insert
- `window.Screen.back`
mock:
```text
notes                                                                      NOR
find or start:
  Ask Priya about the lease renewal  before Friday bring the signed copy  · now
  Check whether the brief was too vague  ↳ Could you clarify which behavior… · 4m
```
```text
notes                              NOR
find or start:
  Ask Priya about the lease renewal
  before Friday bring the signed… · now
  Check whether the brief was too
  vague  ↳ Could you clarify which…
  · 4m

    back       desk       menu      new
```
starts: `Write something down`

## screen Note
answers: What did the user write, and can they add to it now?
shows: the title and whole text of one `Note`.
states:
- empty: `untitled note` and `a note · its first line is its title`; Insert when just started
- loading: no separate loading state
- partial: the title follows the first line while the user types
- error: no separate error state
- full: the note's text, without save controls
keys:
- type: write immediately in a new note
- `i` or tap the text on the phone: edit at that point
- `escape`: return to Normal
- `u`, `shift-u`: undo and redo a text edit
- `tab`, `window.Screen.back`: leave the note; the text is kept without a save step
mock:
```text
Ask Priya about the lease renewal                                         INS
Ask Priya about the lease renewal
before Friday bring the signed copy
```
starts: `Write something down`

## flow Write something down
runs: 2 a week
day: Keep their own memory.
when: something comes to mind that the user must not forget
want: write it down at once, without choosing where it goes
so that: the user finds it again later, in `Notes`
steps:
1. the user chooses `new note`; an empty note opens in Insert
2. the user types; the first line becomes its title
3. the user presses `escape`
4. the user presses `tab` to leave; the note is at the top of `Notes`
system: keeps the text without a save step, and lists the note in `Notes` by when it was last written.
user: every word of it.
cases: the same note open on the laptop and the phone at once, where the newest text wins and text being typed is not replaced
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## flow Reply privately
runs: 2 a week
day: Keep their own memory.
when: a `mail.Mail` needs a remark that is for the user only
want: write it beside the mail
so that: the user sees it the next time they read the thread, and no agent does
steps:
1. the user puts the cursor on the mail
2. the user presses `alt-r`; a private tail opens directly under the mail
3. the user types the remark and presses `escape`; the remark stays under the mail
system: keeps the remark with the mail on every device.
user: the remark.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## rule a note about nothing stays off the desk
when a `Note` is about nothing, then it is listed in `Notes`, ordered by when it was last written, and is not on the `desk.Desk`.

## rule a note about a mail shows in its thread
when a `Note` is about a `mail.Mail`, then the `mail.Thread` that shows the mail shows the note beside it as a `Private reply`.
