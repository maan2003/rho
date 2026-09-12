# A file that will not open no longer lights the lamp

*eng-bgkw, 2026-09-08.*

`Session::open_file` reported a failure by emitting `Signal::Degraded`
directly. It was the only one of the six signal call sites that built a
signal rather than passing one `Health` produced, and that mattered more than
it looks: `Health` owns the reason a recovery takes back, so
`Health::feed_ok` lifts a degraded session only when a reason is set. Raised
from outside, none was, `Signal::Recovered` was never emitted, and rho-gui's
fallen-behind flag — which the workspace ORs into the signal lamp — had
nothing that could clear it.

So an image that would not open, whether from a 404, a missing `xdg-open` or
a full disk, told the user the Slack session had lost touch and lit the lamp
for the rest of the run. A later real outage that recovered would clear it,
since that path does set a reason; absent one it stayed on until rho
restarted.

A file rho could not open says nothing about whether the session is keeping
up, so it is a notice now, the same channel a send that did not happen uses.
What changes for the user: the lamp means what it says again, and a file that
will not open says so once instead of standing in for the health of the whole
session.
