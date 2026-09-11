# DECISION-model-sets-the-pace: The foreground is where the model is looking

Authority: inferred

## Decision

A job is *foreground* when it belongs to the newest cell that registered any
job, and *background* otherwise. Foreground work is what the model is waiting
on; background work is what it moved on from. The distinction is a fact of
registration order, never a guess about what a command is.

Nothing running has urgency of its own. Only three things are events, that is
reasons to send: a job ending (its exit code says whether it failed), a cell
returning with output on it, and `notify()`. Plain output rides along with
whatever sends next and never wakes the model.

Each event waits for company only while foreground work is running. With
nothing foreground running, everything goes at once. While it is:

- a foreground success waits up to 60 seconds for its siblings, so a round of
  parallel commands arrives as one request;
- a failure waits 20 seconds wherever it is: it is news the model can act on;
- a background success waits for the next wake, whatever causes it;
- a `notify()` waits a second, to coalesce with the ones behind it.

The model's check-in (`set_checkin`, default 120 seconds, lasting one turn) is
the most anything waits. It shortens nobody's patience, a turn of prose asks
for none, and `wake_on_tools=False` suppresses every notebook event for that
turn but not the check-in, user input or mail. Patience is measured from when
the scheduler first saw the event while able to act, never from the instant a
tool recorded it, and the reason each request went out is recorded with it
(`WakeFacts` on `Sent` and on Claude `Transcript` rows).

## Rationale

At the moment of the call `npm test` and `npm run dev` are the same call and
nothing can tell them apart, so a classification by kind would be a guess made
at the worst possible time. Registration order is not a guess: the model chose
to move on when it started newer work, and a job it left behind should never
slow down the one it is watching.

Exit codes are the command's own verdict, so failure needs no parsing of output
for crash lines, and a cell that raised has said so itself.

Every wake is a request against a mostly cached context. The loop should run as
slowly as it can while each request still carries something to act on: that is
the whole trade the numbers make, and why finished work is delivered together
rather than one request apiece.
