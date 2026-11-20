# browser

The browser is inside the window. A page is a place like a terminal or a desktop: it fills the window, and the window's own keys stay reachable. The browser runs on the user's device, because it holds their logins; agents have no part in it. This is where the user reads and writes Slack under their own account.

## concept Page
purpose: let the user read and use a web page without leaving the window.
principle: a mail holds a link to a pull request; the user puts the cursor on the link and presses `enter`; the page fills the window; they read it and use `Page.back` to go back to the thread, with the cursor where they left it.
sees: the page, filling the window under the `window.Status line`, which reads `web ›` and the page's title; the lamp still at its right end.
does:
- open `enter` on a link in a `mail.Mail` or in a project's list of threads: open the link as a page; a link that is not a web address echoes that nothing opens it
- open page…, from the palette: ask `page:`, listing the open pages by title, most recently visited first; a title opens that page, anything else is read as a web address, `https://` assumed
- type, click, and scroll: go to the site, except for the window's reserved chords
- palette `ctrl-shift-;`: open `window.Palette` without the site taking the key
- back `ctrl-shift-f9`: `window.Screen.back`, not the site's back
- forward `ctrl-shift-f10`: `window.Screen.forward`, not the site's forward
- close `ctrl-shift-f11`: `window.Screen.close`; the page is gone from both the window's history and the browser

The palette shows `go back`, `go forward`, and `close page` with their keys. On the phone, the bar offers `back`, `desk`, and `menu`; `go forward` and `close page` are in `menu`.

A link in a mail is drawn underlined, in the accent colour. Closing a page closes its tab. A tab no screen shows, such as one the browser restored on its own, is closed after ten minutes. A typed address with no scheme opens as `https://`, except a loopback address or a name with no dot, which open as `http://`. With no browser installed, the echo line reads `page: no browser found`.

## flow Read and reply on a page
runs: 10 a day (estimate)
day: Answer the world.
when: a colleague has written in Slack under the user's own account
want: read what the colleague said and reply without leaving the window
so that: the colleague has their answer and the user can return to their previous work
steps:
1. the user opens Slack as a `Page` from a link or with `Page.open page…`.
2. the user reads the conversation.
3. the user writes a reply in Slack's reply field.
4. the user sends the reply in Slack; their words appear in the conversation.
5. the user uses `Page.back` to return to their work, or `Page.close` if they are done with the page.
system: gives typing and clicks to Slack without a mode change, keeps the window's own keys available, and remembers where the user was.
user: which conversation to answer, and the reply.

## screen Page
answers: which page is this, and what does it say?
shows:
- the current `Page`, filling the reading area, including the site's links and reply fields
- the `window.Status line` with the page title and the lamp when something waits elsewhere
- on the phone, the `window.Phone bar` below the page
states:
- empty: there is no `Page` screen until a page opens
- loading: the window shows the page as soon as the browser has it
- partial: the site can show what has loaded so far
- error: the site can show its own error; if the display cannot draw the page, the last screen stays in view and the echo reads `this display cannot draw a page`
- full: the current page is visible and takes the user's typing, clicks, and scrolls
keys:
- the site's keys, including its typing and shortcuts
- `ctrl-shift-;`: `Page.palette`
- `ctrl-shift-f9`: `Page.back`
- `ctrl-shift-f10`: `Page.forward`
- `ctrl-shift-f11`: `Page.close`
starts: `Read and reply on a page`
mock:
```text
web › tide-team · Slack                                                ●
────────────────────────────────────────────────────────────────────────
# releases

Sam    10:32
Can we ship the clock fix on Friday?

Reply…
```

On the phone:
```text
web › tide-team · Slack       ●
───────────────────────────────
# releases

Sam    10:32
Can we ship the clock fix
on Friday?

Reply…


───────────────────────────────
  ‹ back     ⌂ desk     ☰ menu
```

## concept Bookmark
purpose: let the user return to a page by name, including a Slack page under their own account.
principle: the user bookmarks the Slack page they often need and reaches it by name in `window.Palette`.
sees: a saved page by name in the palette.
does:
- bookmark: choose `bookmark this page…` from an open page, and name the current web address or accept the page’s title
- open: find the bookmark by name in the palette; closing its tab does not remove it

Bookmarks are kept in this device’s GUI database. [A saved bookmark in the phone palette](design/phone-bookmark.png).
