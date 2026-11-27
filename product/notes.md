# notes

Notes are the user's memory: text only they write and only their devices hold. They live apart from the work and can be found from `window.Places`. A note is about nothing and listed in `Notes`, or about a `chat.Message` and also a private reply beside it.

## journey Something to remember tomorrow
goal: the user jots down a private reminder and rho brings it back on time without any agent ever seeing it.
starts when: something comes to mind that the user wants to remember later or tomorrow.
steps:
1. the user runs `new note` in `window.Commands`, types their reminder, and presses `escape`, `Write something down`
2. if they want rho to bring it back at a specific time, they press `space` then `s` and pick a snooze time (`tomorrow · 09:00`, `3 days`, etc.)
3. when that time arrives, the note deals once as a device-only `deal.Card`; the user reads it and marks `done` (`space d`) or snoozes it again (`space s`), `deal.Clear what waits`
done when: the user acted on the reminder and no host or agent ever saw its text.
day: Keep their own memory.

## concept Note
purpose: keep something the user wants to remember, in their own words, where no agent can read it.
principle: the user thinks of something to check tomorrow; they run `new note` from `window.Commands`, type it, and optionally press `space` then `s` for tomorrow morning; the next morning it deals once on their devices, and is always findable in `window.Places` or `Notes`.
sees: its first line, which is its title, or `untitled note`; open, its whole text, to edit like any text (with its scheduled reminder time in `design.quiet` in the note's top-left title row if snoozed).
does:
- new note, from `window.Commands`: open a new, empty note ready to type in Insert; it reads `a note · its first line is its title` until the user types
- edit: type in it; what is typed is kept in under a second after the user stops, and on quit
- remind `space` then `s` (prefilling `:snooze note ` in `window.Commands`): schedule a device-only reminder using the same live-preview snooze durations as `deal.Verdict.snooze`; when due, the note deals once on the user's paired devices (`space d` done clears the reminder, `space s` reschedules it; never seen by any host or agent)
- list notes, from `window.Commands` (or `notes` in `window.Places`): open `Notes`, which lists every note
- open `enter` on a note in `Notes` (or from `window.Places`, jumping to the matched line on full-text search): open the note

No agent sees a note. A note's text and any reminder time pass between the user's devices through attached hosts, sealed, the same way as verdicts: only between devices that hold the user's secret phrase; see `hosts.Pair this device`. A note left empty disappears from the list and the thread. While the note is still open, `u` restores the text and `shift-u` redoes the edit. An unsnoozed standalone note stays off `desk.Desk`; `space` in `NOR` offers `f` places, `s` snooze (reminder), and `u` undo (adding `d` done when currently due as a card).

While the user types a new note, the top line follows its first line as it is typed. A note is saved the moment the user leaves it.

## concept Private reply
purpose: let the user keep a remark on a `chat.Message` in its thread, where only they see it.
principle: a project messages a result the user doubts; they press `alt-r` on the message, type their doubt just beneath it and press `escape`. The remark stays there, dim, marked `🔒 you`.
sees: directly under the message, `🔒 you · time` and the text, dim and slanted; while writing, a line says `── private · only you see this · escape keeps it`.
does:
- reply privately `alt-r` on the message, or `reply privately` from `window.Commands`: start writing beneath it; if the message already has a private reply, edit it there
- edit `enter` or a tap on an existing private reply: write in place; `escape`, or `keep` on the phone, returns to reading

Writing a private reply never settles or alters the thread's `deal.Card`. Opening a private reply from `Notes` or `window.Places` opens its thread at that message.

## concept Find or start
purpose: find a `Note` by words the user remembers, or start one with those words.
principle: the user types `signed copy` and sees the note whose second line says to bring it; the words only become a new note if nothing matches and the user presses Enter.
sees: `find or start:`, the words typed, and only the `Note` rows containing every word, in any order, anywhere in their text.
does:
- search `/` or tap: put the cursor in the line and type to narrow the list, using standard `window.Prompt` editing keys (`ctrl-w`, `ctrl-u`, `ctrl-a`, `ctrl-n`, `ctrl-p`)
- open `enter`: open the selected row; with no match, start a note with the typed text as its first line, in Insert
- leave `escape`: move to the rows without opening either a match or a new note

## screen Notes
answers: What did the user write down, and can they find it or start another note?
shows: `Find or start` at the top; then `Note` rows, last written first, with their first line, a few quieter words from the rest, any reminder time, and age; a `Private reply` adds its message's first words after `↳`, and the thread's subject when different.
states:
- empty: `no notes yet · i or o to start` (on the phone, `no notes yet · tap new to start`)
- loading: no separate loading state; the device lists the notes it holds
- partial: only matching rows; with none, `no match · enter to start a note`
- error: no separate error state
- full: all nonempty notes, private replies too; on return from a note, the list picks that note if it is still there and shows all rows again
keys:
- `enter` or tap a row: open the picked note
- `/` or tap `find or start:`: find by any words in a note (with standard `window.Prompt` editing keys)
- `down` and `up` in the query: pick a match
- `o`, `i` with no notes, or `new` on the phone: start an empty note in Insert
- `space` (`f` places, `u` undo), `space-j` (`ctrl-j`), `tab`, `q`, `:`: `window.Screen`
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
- `space` (`f` places, `s` snooze reminder, `u` undo, plus `d` done when currently due), `space-j` (`ctrl-j`), `tab`, `q`, `:`: leave or act on the note; the text is kept without a save step
starts: `Write something down`, `Something to remember tomorrow`

## flow Write something down
runs: 2 a week
day: Keep their own memory.
when: something comes to mind that the user must not forget
want: write it down at once, without choosing where it goes
so that: the user finds it again later, in `Notes` (or gets a reminder when due)
steps:
1. the user chooses `new note` in `window.Commands`; an empty note opens in Insert
2. the user types; the first line becomes its title
3. the user presses `escape` (and optionally `space` then `s` to schedule a device-only reminder)
4. the user presses `tab` or `space` then `f` to leave; they find the note at the top the next time they open `Notes`
system: keeps the text without a save step, and lists the note in `Notes` by when it was last written.
user: every word of it.
cases: the same note open on the laptop and the phone at once, where the newest text wins and text being typed is not replaced; scheduling a reminder for tomorrow morning.

## flow Reply privately
runs: 2 a week
day: Keep their own memory.
when: a `chat.Message` needs a remark that is for the user only
want: write it beside the message
so that: the user sees it the next time they read the thread, and no agent does
steps:
1. the user puts the cursor on the message
2. the user presses `alt-r`; a private tail opens directly under the message
3. the user types the remark and presses `escape`; the remark stays under the message
system: keeps the remark with the message on every device without affecting the thread's `deal.Card`.
user: the remark.

## rule a note stays off the desk unless a reminder is due
when a `Note` is about nothing and has no due reminder, then it is listed in `Notes`, ordered by when it was last written, and stays off `desk.Desk`. When the user schedules a reminder with `space` then `s`, it deals once on the user's devices at that time.

## rule a note about a message shows in its thread
when a `Note` is about a `chat.Message`, then the `chat.Thread` that shows the message shows the note beside it as a `Private reply`.
