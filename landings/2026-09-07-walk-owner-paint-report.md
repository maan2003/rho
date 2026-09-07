# Slow walk frames report their most expensive scene owners

For every draw above the 4 ms reporting threshold, `rho-qa walk` now prints the
twelve scene owners with the most coarsely attributed paint time. Each line
keeps primitive count, changed-primitive count, and bounds beside `paint_ns`, so
a reviewer can distinguish many cheap primitives from an expensive interval in
one owner. The warm baseline is ranked and printed by the same paint-time
measure.

The timings cover GPUI paint only. They do not allocate prepaint/layout time to
an owner, and their per-primitive boundary attribution is diagnostic rather
than a deterministic oracle. Scene hashes and landing acceptance remain based
on deterministic work and damage checks.
