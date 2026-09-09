# Home is drawn from the replica, with nothing to talk to

The client-store goal was failing where the reader could see it. On a
cold start Home said "nothing needs attention" for as long as the daemon
took to answer — minutes, on the rig — even though this client already
held every verdict the user had given, on its own disk.

`Workspace::new` did read the replica. It could not find it: the client's
database opens on the model thread, and that thread is spawned a few
lines above the read in the same constructor. So the read always ran
before the file was open, and the load that mattered was the one at
attach, which needs a socket.

Waiting for the file there is not allowed: after an unclean stop redb
rebuilds its allocator from every page, 17s on the rig's largest mirror,
and no frame may wait on that. So nothing waits. `desk::on_open` tells
whoever asked when the replica is there, and the workspace holds a task
that reads every host's desk and refreshes Home the moment it is. The
window is up the whole time, and the desk arrives long before a daemon
does.

**Tests.** In rho-mirror: a reader who asks before the open is told at
the open, and one who asks after is told where it stands. In rho-gui: a
workspace with nothing reachable, handed what the replica holds, draws
that note on Home — where before it drew "nothing needs attention". The
rho-gui test hands the desk over rather than installing the global
replica, because that file is the whole process's and a test that
installed one would be writing every other test's verdicts into it.
