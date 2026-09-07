# Landed: the rig says which binaries it launched, and refuses stale ones

Five rig sessions in a row were driven against a `target/profiling/rho-gui`
four hours older than the tree they were attributed to. The rebuilds in
between named `rho-cli` and `rho-daemon`; the rig launches the GUI from a
third binary that nobody rebuilt, and nothing in the rig, the report or the
landing note could have said so. Every number from those five sessions was
withdrawn, and the question they were run to answer is still open.

Nothing was wrong with the measurement. What was missing is that a commit is
a claim about the *tree*, and a profile is a fact about the *binaries*, and
the rig let those two be assumed to be the same thing.

## What it does now

`rig up` prints, before it starts anything, and appends the same block to
`logs/rig.log`:

- the tree commit, as `jj` reports it;
- one line per binary it is about to launch — name, content hash, mtime and
  path;
- the newest source file under `crates/`, `vendor/` or `Cargo.lock`, with its
  time.

If any binary is older than that newest source, `rig up` **refuses to start**.
`--allow-stale-binaries` overrides it, prints `STALE` in the block, records
`stale_binaries` on the session, and `rig status` says so afterwards for as
long as that session is the last one — so an override cannot be forgotten
between the run and the report.

The hash is of the whole file, not of a first block. A first-block hash is
fast and would miss exactly the case this exists for: a rebuild that ran,
reported success, and replaced nothing.

The comparison is scoped to `crates/**`, `vendor/**` and `Cargo.lock`, and the
line says so, because a check whose scope a reader has to guess is a check
they cannot rely on. A markdown file next to a source file changes nothing,
and the test says that in as many words.

## The test shows it failing first

`a_binary_older_than_its_sources_is_seen_as_older` sets mtimes explicitly
rather than sleeping, and asserts **both** answers: older reads as older,
newer reads as newer, and the doc file moves neither. A check that has only
ever been seen to pass has not been shown to do anything, which is the rule
this is an application of.

## What it does not do

It does not know whether the binary was built from *this* tree — only whether
it is older than the tree's newest source. A build from a different tree with
a newer mtime passes. Closing that needs the commit stamped into the binary,
which is a bigger change than this one and is not needed for the fault that
prompted it.
