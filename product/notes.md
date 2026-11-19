# notes

Notes are the user's memory: text only they write and only their devices hold. A note is about nothing, and is then a room of its own on Home, or about a message, and is then a private reply under it.

## bet Notes are untouched by the machine
instead of: notes that agents read, that the product summarises, or that a model helps write.
serves: Keep their own memory.
because: a note holds what no agent may know; its every word is the user's, it never reaches a host in the open, and the machine never changes it.
costs: no agent can act on a note, and the product cannot help with its text.

## concept Note
purpose: keep something the user wants to remember, in their own words, where no agent can read it.
principle: the user thinks of something to check tomorrow; they run `new note` and type it; it is kept a moment after they stop typing, and Home lists it as a room tagged `private`.
sees: its first line, which is its title, or `untitled note`; open, its whole text, to edit like any text.
does:
- new note, from the palette: open a new, empty note with no question asked; it reads `a note · its first line is its title` until the user types
- edit: type in it; what is typed is kept in under a second after the user stops, and on quit
- open: from Home, from `window.Find`, or with `enter` on a private reply

No agent sees a note. A note's text passes between the user's devices through the hosts, sealed, the same way as verdicts: only between devices that hold the user's secret phrase, which the window cannot enter. The window has no way to delete a note.

While the user types a new note, the top line still reads `untitled note`; it shows the first line once the note is opened again.

## concept Private reply
purpose: let the user keep a remark on a message in its thread, where only they see it.
principle: an agent reports a result the user doubts; they put the cursor on it and run `reply privately`; a note opens; they type their doubt; back in the thread, it shows under the message, dim, marked `🔒 you`.
sees: in the thread, under its message: `🔒 you · time` and the text, dim and slanted.
does:
- reply privately, from the palette: start a note about the thing under the cursor and open it; on a note, the new note is about that note
- open `enter` on it in the thread: open the note to edit it

## screen Note
shows: `Note`
starts: `Write something down`

## job Write something down
when: something comes to mind that I must not forget
want: write it down at once, without choosing where it goes
so that: I find it again later, on Home or by name
flow:
1. open the palette and choose `new note`; an empty note is on screen
2. press `i` and type; the first line is its title
3. leave with `escape` and `tab`; the note is a room on Home, tagged `private`
system: keeps the text without a save step, and lists the note on Home by when it was last written.
user: every word of it.
cases: the same note open on the laptop and the phone at once, where the newest text wins and text being typed is not replaced

## job Reply privately
when: a message in a thread needs a remark that is for me only
want: write it beside the message
so that: I see it the next time I read the thread, and no agent does
flow:
1. put the cursor on the message
2. open the palette and choose `reply privately`; a new note opens
3. type the remark
4. `window.Screen.back`; the remark shows under the message
system: keeps the remark with the message on every device.
user: the remark.

## rule a note about nothing is a room
when a `Note` is about nothing, then it is a `home.Room` on Home, tagged `private`, ordered by when it was last written.

## rule a note about a message shows in its thread
when a `Note` is about a `chat.Message`, then the `chat.Thread` that shows the message shows the note under it as a `Private reply`.
