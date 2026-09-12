# Landed: a rig report carries the drive's name and step count

One commit produced 16%, 79% and 4.9% of frames over the 4 ms bar in three
sessions. Read as a table those three rows look like a trend, and there is no
trend in them: the difference is neither the commit nor the machine, it is
what the reader did. Nothing in the rig recorded that, so no row could be
compared with any other row, and the table's shape invited exactly the reading
it could not support.

## What it does now

The driver writes one line per thing it does — every `key`, `input`, `type`,
`click` and `move` — to `<session>-drive.log`. The log sits **beside** the
wayland session directory rather than inside it, because `stop` removes the
directory and the log is the part that has to outlive the run.

`rho wayland --session <s> drive "<name>"` names the drive that follows. Steps
after it are counted against that name; a later name starts the count again,
so a session that runs two recipes reports the one whose frames are in the
profile.

`rig down` reads the log, prints `drive <name>, N steps`, files the log into
`logs/` under the profile's own stem — so a run's numbers, its errors and the
steps that produced both share one name — and stores the pair on the session
so `rig status` says it afterwards. `rig up`'s hint line now names the `drive`
command first, before the keys.

**A run with no drive named is reported as having none, in words.** That is
the case the change exists for. A blank there reads as "the usual recipe",
and a number nobody can attribute to a recipe is a number nobody can compare.

Recording a step can never fail a step: every error on that path is dropped
deliberately. A driver that refuses to press a key because it could not write
a log line is worse than a log with a gap in it.

## The test

`steps_are_counted_under_the_drive_they_ran_for` reads the same log twice,
once without a `drive` line and once with, and asserts both counts — 2 and 3 —
so the reset is shown to do work rather than the count happening to be right.
It also asserts that a line which is neither a step nor a name counts as
neither, and that a missing log is the third answer rather than zero steps.

## What it does not do

It counts steps; it does not compare them. Two runs both called "the 09:12
recipe" with different step counts are two different drives whatever they are
called, and the count beside the name is what makes that visible. Naming
drives consistently is still a habit and not something the rig can enforce.
