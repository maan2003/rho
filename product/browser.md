# browser

The browser is inside the window. A page is a place like a terminal or a desktop: it fills the window, and the window's own keys stay reachable. The browser runs on the user's device, because it holds their logins; agents have no part in it. This is where the user reads and writes Slack under their own account.

## concept Page
purpose: let the user read and use a web page without leaving the window.
principle: a message holds a link to a pull request; the user puts the cursor on the link and presses `enter`; the page fills the window in `INS`; they read it, press `escape` to enter rho `NOR`, and press `ctrl-k` to return to the thread with the cursor where they left it.
sees: the page, filling the window under the `window.Status line`, which reads `web ›` and the page's title, with `INS` while live and `NOR` when escaped; the lamp still at its right end.
does:
- open `enter` on a link in a `chat.Message`: open the link as a page; a link that is not a web address echoes that nothing opens it
- open page…, from `window.Palette`: ask `page:`, listing the open pages by title, most recently visited first; a title opens that page, anything else is read as a web address, `https://` assumed
- live input (`INS`): type, click, and scroll go to the site; `escape` enters rho `NOR`; `f9` sends a literal `escape` to the page; `ctrl-shift-;` opens `window.Palette` in one step
- normal (`NOR`): standard `window.Screen` keys (`ctrl-k` back, `ctrl-j` forward/deal, `tab` desk, `q` close page, `:` palette); `i` or clicking the page returns to `INS`

A page is never a dealable `deal.Card`; `space` never offers `done`, `mute`, or `snooze` for a page.

A link in a message is drawn underlined, in the accent colour. Closing a page closes its tab. A tab no screen shows, such as one the browser restored on its own, is closed after ten minutes. A typed address with no scheme opens as `https://`, except a loopback address or a name with no dot, which open as `http://`. With no browser installed, the echo line reads `page: no browser found`.
rejected:
- dedicated `ctrl-shift-f9` / `ctrl-shift-f10` / `ctrl-shift-f11` bindings for back, forward, and close · 2026-11-26 · replaced by `window.raw places share one dual-escape model`

## flow Read and reply on a page
runs: 10 a day (estimate)
day: Answer the world.
when: a colleague has written in Slack under the user's own account
want: read what the colleague said and reply without leaving the window
so that: the colleague has their answer and the user can return to their previous work
steps:
1. the user opens Slack as a `Page` from a link, a `Bookmark`, or `Page.open page…`.
2. the user reads the conversation.
3. the user writes and sends a reply in Slack's reply field.
4. the user presses `escape` to enter rho `NOR`, then `ctrl-k` to return to their work or `q` to close the page.
system: opens in `INS` so typing and clicks go to Slack immediately, keeps `escape` (rho `NOR`), `f9` (page `escape`), and `ctrl-shift-;` (`Palette`) available, and remembers where the user was.
user: which conversation to answer, and the reply.

## screen Page
answers: which page is this, and what does it say?
shows:
- the current `Page`, filling the reading area, including the site's links and reply fields
- the `window.Status line` with `web ›` and the page title, true mode (`INS` or `NOR`), and the lamp when something waits elsewhere
- on the phone, the `window.Phone bar` below the page
states:
- empty: there is no `Page` screen until a page opens
- loading: the window shows the page as soon as the browser has it
- partial: the site can show what has loaded so far
- error: the site can show its own error; if the display cannot draw the page, the last screen stays in view and the echo reads `this display cannot draw a page`
- full: the current page is visible and takes the user's typing, clicks, and scrolls
keys:
- in `INS`: the site's keys; `escape` enters `NOR`; `f9` sends literal `escape` to the site; `ctrl-shift-;` opens `window.Palette`
- in `NOR`: standard `window.Screen` keys (`ctrl-k`, `ctrl-j`, `tab`, `q`, `:`); `i` or click returns to `INS`
starts: `Read and reply on a page`

## concept Bookmark
status: currently stored locally per device without a remove command.
purpose: let the user return to a page by name, including a Slack page under their own account, from any paired device.
principle: the user bookmarks the Slack page they often need and reaches it by name in `window.Palette` on both laptop and phone.
sees: a saved page by name in `window.Palette`.
does:
- bookmark this page…, from `window.Palette`: uses the open page or prompts for an address, and saves it under the chosen name or page title
- remove bookmark…, from `window.Palette`: removes a saved bookmark (prompting which one, or offering the current page's bookmark first)
- open: find the bookmark by name in `window.Palette`; closing its tab does not remove it

Bookmarks sync across paired devices through the user's sealed ledger, the same way as verdicts and notes; see `hosts.Pair this device`.
captures: [a saved bookmark in the phone palette](design/phone-bookmark.png).
