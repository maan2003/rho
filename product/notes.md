# notes

Notes are the user's memory: text only they write and only their devices hold. They live apart from the work and can be found from the user's palette. A note is about nothing and listed in `Notes`, or about a `mail.Mail` and also a private reply beside it.

## concept Note
status: the user can list notes and find them by name; the palette does not search their full text.
purpose: keep something the user wants to remember, in their own words, where no agent can read it.
principle: the user thinks of something to check tomorrow; they run `new note` and type it; it is kept a moment after they stop typing; the next day they run `notes`, and it is at the top of the list.
sees: its first line, which is its title, or `untitled note`; open, its whole text, to edit like any text.
does:
- new note, from the palette: open a new, empty note with no question asked; it reads `a note · its first line is its title` until the user types
- edit: type in it; what is typed is kept in under a second after the user stops, and on quit
- notes, from the palette: open `Notes`, which lists every note
- open `enter` on a note in `Notes`, or on a private reply: open the note

No agent sees a note. A note's text passes between the user's devices through the hosts, sealed, the same way as verdicts: only between devices that hold the user's secret phrase; see `hosts.Pair this device`. The window has no way to delete a note.

While the user types a new note, the top line follows its first line as it is typed. A note is saved the moment the user leaves it.

## concept Private reply
purpose: let the user keep a remark on a `mail.Mail` in its thread, where only they see it.
principle: a project mails a result the user doubts; they put the cursor on it and run `reply privately`; a note opens; they type their doubt; back in the thread, it shows under the mail, dim, marked `🔒 you`.
sees: in the thread, under its message: `🔒 you · time` and the text, dim and slanted.
does:
- reply privately, from the palette: when the cursor is on a mail or thread, start a note about it and open it; the command does not appear on the desk or with nothing under the cursor
- open `enter` on it in the thread: open the note to edit it

## screen Notes
status: a private reply's row names a host chat message and thread rather than a project mail and subject.
shows: `Note`, by its first line, the last written first, private replies too; with none, `no notes yet · new note from the palette (:)`; a private reply names its mail's first words and the thread's subject
starts: `Write something down`

## screen Note
shows: `Note`
starts: `Write something down`

## flow Write something down
runs: 2 a week
day: Keep their own memory.
when: something comes to mind that the user must not forget
want: write it down at once, without choosing where it goes
so that: the user finds it again later, in `Notes`
steps:
1. the user opens the palette and chooses `new note`; an empty note is on screen
2. the user presses `i` and types; the first line is its title
3. the user leaves with `escape` and `tab`; the note is at the top of `Notes`
system: keeps the text without a save step, and lists the note in `Notes` by when it was last written.
user: every word of it.
cases: the same note open on the laptop and the phone at once, where the newest text wins and text being typed is not replaced
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## flow Reply privately
status: the user makes the reply beside a message in chat; the mail thread is not built.
runs: 2 a week
day: Keep their own memory.
when: a `mail.Mail` needs a remark that is for the user only
want: write it beside the mail
so that: the user sees it the next time they read the thread, and no agent does
steps:
1. the user puts the cursor on the mail
2. the user opens the palette and chooses `reply privately`; a new note opens
3. the user types the remark
4. the user goes back with `window.Screen.back`; the remark shows under the mail
system: keeps the remark with the mail on every device.
user: the remark.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## rule a note about nothing stays off the desk
status: notes already have their own list and are never rooms on Home; the desk does not exist yet.
when a `Note` is about nothing, then it is listed in `Notes`, ordered by when it was last written, and is not on the `desk.Desk`.

## rule a note about a mail shows in its thread
status: a private note shows beside its chat message in a host thread.
when a `Note` is about a `mail.Mail`, then the `mail.Thread` that shows the mail shows the note beside it as a `Private reply`.
