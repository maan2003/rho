# browser

The browser is inside the window. A page is a place like a terminal or a desktop: it fills the window, and the window's own keys stay reachable.

## concept Page
status: not built. A link in a message opens in the device's own browser, outside the window.
purpose: let the user read and use a web page without leaving the window.
principle: a message holds a link to a pull request; the user presses `enter` on it; the page fills the window; they read it and press `ctrl-k` to go back to the thread.
sees: the page, filling the window under the `window.Status line`; its title at the top.
does:
- open: `enter` on a link in a message, or by name from the palette
- every key, click, and wheel: goes to the page, as on an `agents.Desktop`
- palette `ctrl-shift-;`: open the palette without the page seeing the keys

The browser runs on the user's device, because it holds their logins. Agents are not part of it.
