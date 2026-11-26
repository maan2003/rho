# usage

Usage shows what the agents have spent and how much of each account's allowance is left, so the user can see when they are about to run out.

## concept Chart
purpose: show the agents' spending or allowance over the last week or month.
principle: agents slow down; the user opens `rate limit · 7d` from `window.Places` (or `open usage` from `window.Commands`) and reads that one Claude account has 18% left and another has plenty; they run `switch claude account` and both running and new Claude agents immediately use the account with room.
sees: the chart's name and how far back it looks; the answer in words; the picture, with the days named under it.
does:
- open, from `window.Places` with `usage` (or `open usage` from `window.Commands`): open a chart (typing `usage` puts `rate limit · 7d` first)
- switch window or chart in place: change between `7d` and `30d`, or switch among the four pictures, keeping one `Chart` screen rather than opening separate screens
- switch claude account, from `window.Commands` anywhere (inline or prompt): pick an attached host (when more than one) and switch its active Claude account; **both already-running Claude agents (on their next request) and newly started Claude agents** use the switched account so stalled work unblocks immediately. (GPT account switching is already automatic on the host, so no manual GPT switch command is needed.)

The four pictures are:
- `rate limit`: how much of each account's allowance is left, over time
- `model cost`: what each model has cost, added up over the window, with only models that spent something named above the picture
- `model usage share`: which models did the work recently, and how much work there was
- `GPT agent cost`: how much a typical GPT agent costs and how much the costlier ones cost

A usage chart is never a dealable `deal.Card` (`space` in `NOR` offers `f` places and `u` undo only).

## screen Chart
answers: How much allowance is left on each account, and when does it reset?
shows:
- `Chart`: its name and window, then `design.Allowance row` for each account (marking the active account with `★`), or the spending totals and the models that contributed
- `Chart`: the picture with valued marks at the left and named days below, above the `design.Phone bar` on the phone
states:
- empty: `no account has reported its allowance yet`, or `nothing spent in the last 7 days` (or 30); no empty picture
- loading: `waiting for the agent hosts' answer`
- partial: a known account without a report says `allowance not reported` beside accounts that have reported
- error: `no agent host is attached`; an away host is noted quietly on `desk.Desk` (or in red on this screen if it is the only attached host)
- full: the allowance rows or spending totals, then the picture
keys:
- `] w`, `[ w`: switch between `7d` and `30d` in place
- `] v`, `[ v`: switch to the next or previous chart in place
- `space` (`f` places, `u` undo), `space-j` (`ctrl-j`), `tab`, `q`, `:`: `window.Screen`
starts: `Check what is left`

## flow Check what is left
runs: 2 a week
day: Keep the work moving.
when: agents seem slow, or the user is about to start many of them
want: see how much allowance each account has left and when it resets
so that: the user moves Claude work onto an account with room (GPT accounts already rotate automatically on the host)
steps:
1. the user opens `window.Places` and chooses `rate limit · 7d` (or runs `open usage` from `window.Commands`)
2. the user reads the allowance rows; the picture shows how quickly the allowance has fallen
   - if they want a wider view or spending breakdown, they switch to `30d` or another chart in place
   - if a Claude account is low, they run `switch claude account` from `window.Commands`, which unblocks already-running Claude agents on their next request as well as new ones
system: asks every attached host when the chart opens, and again when a host comes back, and puts their answers together; rotates GPT accounts automatically and applies Claude account switches to running and new agents alike.
user: which chart, and whether to switch the active Claude account.
cases: no account has reported; an account has not reported a reset time; a reset time has passed since the last report; running Claude agents blocked on an empty account unblock immediately after `switch claude account`.
