# browser

The browser is inside the window. A page is a place like a terminal or a desktop: it fills the window, and the window's own keys stay reachable. The browser runs on the user's device, because it holds their logins; agents have no part in it. This is where the user reads and writes Slack under their own account.

## concept Page
status: web links open from mail and the way back from a page goes to Home rather than the desk. There are no rho `Bookmark`s.
purpose: let the user read and use a web page without leaving the window.
principle: a mail holds a link to a pull request; the user puts the cursor on the link and presses `enter`; the page fills the window; they read it and press `ctrl-k` to go back to the thread, with the cursor where they left it.
sees: the page, filling the window under the `window.Status line`, which reads `web ›` and the page's title; the lamp still at its right end.
does:
- open `enter` on a link in a `mail.Mail` or in a project's list of threads: open the link as a page; a link that is not a web address echoes that nothing opens it
- open page…, from the palette: ask `page:`, listing the open pages by title, most recently visited first; a title opens that page, anything else is read as a web address, `https://` assumed
- every key, click, and wheel: goes to the page
- palette `ctrl-shift-;`: open the palette without the page seeing the keys; see `window.Palette`
- back `ctrl-k`, forward `ctrl-j`, close `ctrl-shift-backspace`: as on any `window.Screen`; the desk is reached through the palette, as from an `agents.Desktop`

A link in a mail is drawn underlined, in the accent colour. Closing a page closes its tab. A tab no screen shows, such as one the browser restored on its own, is closed after ten minutes. A typed address with no scheme opens as `https://`, except a loopback address or a name with no dot, which open as `http://`. With no browser installed, the echo line reads `page: no browser found` and says where the window looks for one.

## concept Bookmark
status: not built; the user's Slack and other pages open in the browser, but there are no page bookmarks in the rho palette.
purpose: let the user return to a page by name, including a Slack page under their own account.
principle: the user bookmarks the Slack page they often need and reaches it by name in `window.Palette`.
sees: a saved page by name in the palette.
does:
- bookmark: save a `Page` for another visit
- open: reach the saved page by name
