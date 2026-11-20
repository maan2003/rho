# usage

Usage shows what the agents have spent and how much of each account's allowance is left, so the user can see when they are about to run out.

## concept Chart
purpose: show the agents' spending or allowance over the last week or month.
principle: agents slow down; the user opens `rate limit · 7d` and reads that one account has 18% left and another has plenty; they start the next agent on an account with room.
sees: the chart's name and how far back it looks; the answer in words; the picture, with the days named under it.
does:
- open, from the palette with `usage`: one of eight charts, the four pictures over 7 days or 30 days

The four pictures are:
- `rate limit`: how much of each account's allowance is left, over time
- `model cost`: what each model has cost, added up over the window, with only models that spent something named above the picture
- `model usage share`: which models did the work recently, and how much work there was
- `GPT agent cost`: how much a typical GPT agent costs and how much the costlier ones cost

Typing `usage` puts `rate limit · 7d` first.

## screen Chart
answers: How much allowance is left on each account, and when does it reset?
shows:
- `Chart`: its name and window, then `design.Allowance row` for each account, or the spending totals and the models that contributed
- `Chart`: the picture with valued marks at the left and named days below, above the `design.Phone bar` on the phone
states:
- empty: `no account has reported its allowance yet`, or `nothing spent in the last 7 days` (or 30); no empty picture
- loading: `waiting for the agent hosts' answer`
- partial: a known GPT account without a report says `allowance not reported` beside accounts that have reported
- error: `no agent host is attached`; a lost host is named by `window.Status line`
- full: the allowance rows or spending totals, then the picture
keys:
- `window.Palette` to open another chart by name
- `window.Screen` to read, search or go back
starts: `Check what is left`
mock:
```text
usage › rate limit · 7d                                                         NOR
rate limit · last 7 days
gpt/home  18% left · resets in 2.1d
★ gpt/work  66% left · resets in 5.0d

100%  ────────────────────────────────────────────────────────────────────────────
      ╲
 50%  ─╲─────────────────────────────────────────────────────────────────────────
       ╲
  0%  ────────────────────────────────────────────────────────────────────────────
       Sat        Sun         Mon        Tue        Wed        Thu        Fri
```

On the phone the same rows and named days fit above `design.Phone bar`.

## flow Check what is left
runs: 2 a week
day: Keep the work moving.
when: agents seem slow, or the user is about to start many of them
want: see how much allowance each account has left and when it resets
so that: the user puts new work on an account that can take it
steps:
1. the user opens the palette and chooses `usage › rate limit · 7d`
2. the user reads the allowance rows; the picture shows how quickly the allowance has fallen
system: asks every attached host when the chart opens, and again when a host comes back, and puts their answers together.
user: which chart, and what to do about it.
cases: no account has reported; an account has not reported a reset time; a reset time has passed since the last report; a host is slow to answer.
reviewed: 2026-11-20 · laptop · reviewer · 3 findings
