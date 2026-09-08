# A tool call's line, without what the tool said

The transcript draws a tool call — what ran, its arguments, its status and how
long it took — and no longer draws its output. The body is not in the
document: not rendered, not composed, and not asked for.

What a reader sees change: a call that used to sit above an indented block of
its output, or above the twelve-row tail of one, is now one line. Nothing is
folded away, because nothing is there to fold. A call still running, a call
that failed and a call that finished all read the same way they did before on
their own line; the status and the duration are unchanged.

Why now: the output was fetched per composed chunk and spliced into the
document, and the elision that was supposed to hide it kept a twelve-row tail
on any turn that never produced text. That tail was invisible while a fold
held only call lines and became a window onto output the moment bodies were
drawn under them, which is the folding the user reported. Rather than tune the
policy under the reader, the body comes out of the document entirely until
there is a rule that holds.

What it costs, on the gate's own drive, same box, counts rather than timings:
the largest event's touched rows fall from 600 to 308, the largest changed
count from 5,443 to 3,772, and the elided drive's settling step draws 1,911
primitives across 151 owners against 2,767 across 161. On a real transcript
the saving is larger than that and is mostly not rows: a composed chunk sent
one request naming every position its calls came from and then spliced the
answers in, and the results in the population measured on 2026-09-07 average
4,047 bytes with a p90 of 13,097 and a maximum of 170,448. None of those bytes
enter the document now, and no request goes out.

Nothing is lost from the host. The daemon still holds every result, still
answers a Detail request for one, and `full_output` still keeps the complete
body — the transcript simply does not ask. When output comes back, it comes
back with an elision rule that says which one output is visible.
