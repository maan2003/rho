# The channel list says which day, not just which hour

*eng-bgkw, 2026-09-08.*

Every row of the Slack listing ended with a wall clock and nothing else. A
channel whose last message was Friday afternoon read `17:32` on Monday
morning, sitting beside one that had spoken ten minutes earlier and reading
exactly the same. Nothing else on the row carried a day: the label, the
mention count, the unread, the watched mark, the time, and that was the row.

The transcript already knew better — it breaks between days with
`day_label` — so the surface the reader scans to decide what to open was the
one surface that could not tell them when.

What changes on screen: today is still the clock, the last week is the
weekday (`Mon`), further back is the date (`1 Sep`), and a year that is not
this one says so (`8 Sep 2025`). The words are `day_label`'s, and the day
boundary is `crosses_day`'s — local calendar days rather than a count of
seconds — so the list and the transcript cannot disagree about where a day
starts.
