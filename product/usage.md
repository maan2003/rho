# usage

Usage shows what the agents have spent and how much of each account's allowance is left, as charts, so the user can see when they are about to run out.

## concept Chart
purpose: show one picture of the agents' spending or allowance over the last week or month.
principle: agents slow down; the user opens `rate limit · 7d` and sees one account at 18% and falling, and another with plenty left; they start the next agent on a runtime of the other model.
sees: its name and how far back it looks, such as `rate limit · last 7 days`; for spending, a total such as `$… estimated API cost · … requests`; the picture, with a legend over it and the days under it.
does:
- open, from the palette with `usage`: one of eight charts, the four pictures over 7 days or 30 days

The four pictures are:
- `rate limit`: how much of each account's allowance is left, over time; the legend gives each account, the share left now, and the days until it resets, such as `opus 66% · 5.0d`; a `★` marks the GPT account in use now
- `model cost`: what each model has cost, added up over the window
- `model usage share`: which models did the work, as shares, smoothed
- `GPT agent cost`: how the cost spreads over the agents

Typing `usage` puts `rate limit · 7d` first.

## screen Chart
shows: `Chart`
starts: `Check what is left`

## flow Check what is left
runs: 2 a week
day: Keep the work moving.
when: agents seem slow, or the user is about to start many of them
want: see how much allowance each account has left and when it resets
so that: the user puts new work on an account that can take it
steps:
1. the user opens the palette and chooses `usage › rate limit · 7d`
2. the user reads the legend and the lines
system: asks every attached host when the chart opens, and again when a host comes back, and puts their answers together.
user: which chart, and what to do about it.
cases: no host attached, where the chart reads `no agent host is attached`; no account on any host, where it reads `no accounts on lab`; an account that has read nothing yet, where it reads `no usage yet`; a host slow to answer, where it reads `waiting for the agent hosts' answer`
reviewed: 2026-11-20 · laptop · reviewer · 3 findings
