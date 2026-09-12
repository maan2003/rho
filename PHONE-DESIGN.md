# Rho on the phone

Status: design, decided with the user on 2026-09-03. Companion to
`DESK-DESIGN.md`. The web build is gone; the phone runs the native app under
Wayland, so everything here is the native GUI in narrow layout
(`phone_mode`, width at most 600px or `RHO_PHONE=1`).

## The problem

The phone build was a projection of the desktop: vim modes, a bottom bar of
`back · menu · send`, and a two-finger swipe to deal. On the device none of
the swipes do anything, the bar reads badly, and modal editing has no place
on a touch screen. The user's words: make it feel like TikTok, match the up
and down movements the desktop already has, no vim on the phone, and
redesign the bar.

## Decisions

### The phone is a feed of cards, one card per screen

Dealing is the phone's home. The screen is the current deal card: a one-line
header (the deal path on the left, the state and age on the right, the same
words as the desktop deal bar), the card body below it, the verdict bar at
the bottom. There is no desk map to orient on first; the desk is one of the
surfaces reachable from the menu, not the root.

### One finger moves between cards, the way TikTok does

- A vertical flick when the body cannot scroll further in that direction
  moves cards: flick up at the bottom of the body (or on a body that fits
  the screen) opens the next deal; flick down at the top goes back to the
  previous card (the same step as `shift-u` when the previous card was
  closed by a verdict, history back otherwise).
- Inside the body, one finger scrolls the body as normal. The card only
  moves when the scroll is already at its end and the flick has velocity.
- The physical direction matches the desktop trackpad gesture that opens
  the next deal: the same finger motion does the same thing on both. If the
  desktop gesture turns out to be the opposite motion, the desktop flips to
  match the phone, not the other way round, and the change is reported.
- Cards animate with the finger: the next card follows the drag and snaps
  when released past a third of the screen or with velocity.
- Two-finger gestures are removed. Nothing on the phone needs two fingers.

### No vim on the phone

Every editor on the phone is in insert mode all the time. There is no normal
mode, no mode word in any bar, no `escape` contract. Read-only surfaces
(transcripts, conversations, the desk when not editing) scroll and tap; a
tap on an editable region places the cursor and raises the keyboard; a tap
outside dismisses it. Deal mode is not a vim mode on the phone: it is the
feed. Keys still work when a hardware keyboard is attached, but nothing
depends on them.

### The bar is verdicts, with big targets

The bottom bar is one row of equal-width targets at least 48px tall, icon
above a short label, inside the safe area, no status text, no percentages.

- On a deal card: `done`, `dismiss`, `defer`, `todo`, `file`, `reply`, in
  that order. `reply` raises the keyboard into the card's composer (agent
  prompt, Slack composer); the send action lives on the keyboard row, not
  in the bar. A verdict animates the card away and shows the next one.
- On any other surface: `back`, `menu`, and the surface's own primary
  action if it has one (`send` in a composer, `edit` on the desk).
- The menu is a bottom sheet with large rows: desk, Slack, agents, status;
  no key hints on the phone.

The top header is one line, muted, never a second bar.

### Touch that can be diagnosed

Every committed gesture is a typed journal event (`PhoneFlick { direction,
moved_card: bool }`, `PhoneVerdict { verdict }`), and a debug overlay
(`RHO_PHONE_TOUCH_DEBUG=1`) shows the live contact count and the last
gesture so a report from the device can say what the app saw. The
gesture recogniser is unit tested with injected `TouchEvent`s, including the
scroll-at-end rule and the velocity rule.

## What stays the same

Surfaces, the dealer, verdict keys and undo, the journal, the desk. The
desktop is untouched by this design except the gesture direction check.

## Deliberately deferred

- Notifications on the phone.
- Voice.
- Multi-card overview on the phone.
