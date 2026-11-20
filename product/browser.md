# browser

The browser is inside the window. A page is a place like a terminal or a desktop: it fills the window, and the window's own keys stay reachable. The browser runs on the user's device, because it holds their logins; agents have no part in it.

## concept Page
purpose: let the user read and use a web page without leaving the window.
principle: a message holds a link to a pull request; the user puts the cursor on the link and presses `enter`; the page fills the window; they read it and press `ctrl-k` to go back to the thread, with the cursor where they left it.
sees: the page, filling the window under the `window.Status line`, which reads `web ›` and the page's title; the lamp still at its right end.
does:
- open `enter` on a link in a `chat.Message`, in a room's list of threads, or in a Slack message: open the link as a page; a link that is not a web address echoes that nothing opens it
- open page…, from the palette: ask `page:`, listing the open pages by title, most recently visited first; a title opens that page, anything else is read as a web address, `https://` assumed
- every key, click, and wheel: goes to the page
- palette `ctrl-shift-;`: open the palette without the page seeing the keys
- back `ctrl-k`, forward `ctrl-j`, close `ctrl-shift-backspace`: as on any `window.Screen`; home is reached through the palette, as from an `agents.Desktop`

A link in a message is drawn underlined, in the accent colour. A closed page's tab lives on for ten minutes, so `ctrl-j` brings it back as it was.
