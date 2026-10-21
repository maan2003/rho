# Proof techniques

Use these when the proof is hard to get. They fall into three groups:

- **Running the change the way users reach it**: External services, Failures and timing, and Environments and states. That may need an external API without a usable sandbox, a hung process, a second machine, a native app shell, or a timing window of milliseconds. Before falling back to a unit test, build the missing piece. Each technique replaces one unreachable part and keeps everything else real.
- **Using production**: the Production section, which covers what to do both when the product allows it and when it does not.
- **Strengthening the proof**: Comparing against a known-correct answer, Rules that must always hold, Behavior that varies between runs, Simulated users, and Observing what the user cannot see. These go beyond a few hand-checked cases.

## External services

- Use the vendor's local tooling when it exists: an official mock server, a CLI that forwards and triggers webhooks, a local emulator of a cloud service, an S3-compatible object store, or an SMTP sink that captures mail.
- Otherwise build a fake of the part of the API the code uses: a local HTTP server with realistic responses, state, errors, pagination, and idempotency, which sends webhooks signed with the vendor's real signing scheme. Point the app at it through its existing base-URL configuration. A fake Stripe, for example, creates customers and subscriptions, declines a card on request, and posts signed `invoice.payment_failed` events.
- Ground the fake in the vendor's published API spec or in recorded real responses.
- Check the fake against the real service before trusting it. Send the same calls to the vendor's sandbox and to the fake, diff the status codes, bodies, and webhook payloads, and fix the fake until the calls the code makes show no differences.
- Use the fake for what the real service forbids: thousands of runs without rate limits, abuse detection, or cost, and failures that are dangerous to trigger for real, such as outages, partial writes, and malformed responses.
- Name the faked boundary in the report. A fake proves the code against your model of the vendor, not the vendor itself.
- For sign-in, run a local mock identity provider and complete the real redirects in a browser.
- Replace a dependency whose output varies, such as a language model, with a scripted fake when the change is not about that dependency, so that both runs get identical input.

## Failures and timing

- Freeze a dependency with `kill -STOP` and resume it with `kill -CONT` to simulate a hang. Kill it with `kill -9` to test crash recovery.
- Delay, drop, or fail chosen requests with browser request interception or a proxy that adds latency and resets connections.
- Force the bad order of events with a gate in the code under test, or with a delay injected identically in both runs.
- Fake the clock, or rewind a poller's cursor, to reach expiry, retry, and polling windows without waiting.
- Cause the failure on purpose: fill the disk, make a config file read-only, revoke a permission, or expire a token.

## Environments and states

- Pin the whole system under test, including the operating system, services, and tool versions, with a lockfile, a container image, or Nix, so that the before and after runs differ only by the change.
- Simulate a second machine or client with a separate home or state directory or browser profile. Use a second real account for permission and multiplayer cases.
- Build the old and new versions side by side to test mixed versions, such as an old client against a new server.
- Recreate the reporter's environment: the repository shape (no remote, a stale remote default branch, a path with spaces), the data volume (the same message count), and the OS settings (text size, theme, locale).
- When a native app wraps web content, inject the same bridge script the native shell runs into a browser to reproduce the shell's state, then confirm on a real device, or on a simulator when no device is available.
- Seed the database with production-shaped data. For query plans, scale the planner statistics to production size.
- To test with real data, copy it one way into an isolated state directory or database. Never point a test run at live user state, and never copy test state back.

## Production

Production has the real data, load, and users, so evidence from it is the strongest. Whether you can use it depends on the product, the access you have, and the user's permission. Reading is the default. Ask before anything that changes production, and never change real users' data to prove a change.

When the product allows it, from least to most invasive:

- **Read what production records**: logs, metrics, traces, error reports, and query statistics, through read-only credentials. Measure how often the bug happens before the change, and show the new code path running after it.
- **Test without changing anything**: run the new query or its plan against a read replica, or create an index inside a transaction, measure the query, and roll the transaction back.
- **Run the new code next to the old**: compute the new result next to the old one and log every difference, or run a new check in log-only mode. Switch to enforcing only after the logs show no false positives.
- **Gate it**: ship behind a feature flag enabled only for test accounts and reproduce the scenario there. Then widen the rollout in steps while watching error rates and latency, with a rollback ready.
- **Add diagnostics**: logging, metrics, or read-only admin endpoints whose only purpose is to let an agent see what production did.

Many products cannot be tested in production: software that customers install or host, such as desktop, mobile, embedded, and on-premises products; regulated systems and systems that move money; and services without telemetry or without access for you. Get as close as the product allows:

- Replay recorded production traffic, traces, or incidents against the new build, after removing personal data.
- Test against an anonymized or production-shaped copy of the data at production volume.
- Ship to a channel that users opt into, such as a beta, nightly, or TestFlight build, or a small rollout percentage, and read its crash and error reports.
- Send the reporter a test build to confirm the fix, or read the logs or diagnostic bundle they provide.

In the report, name which of these you used and what production evidence is still missing.

## Comparing against a known-correct answer

When something independent already gives the right answer, compare against it across many inputs instead of checking a few cases by hand.

- Possible sources of the right answer: the code before the change (for a refactor or an optimization), an upstream library, another implementation of the same format or protocol, the production service, or a reference implementation's snapshot or conformance tests.
- Generate the inputs: a property-based testing library, a fuzzer, a corpus of real inputs such as recorded requests, or every combination of a small set of options. Feed each input to both sides and diff the outputs.
- Reuse the reference's own test suite when it has one: parse its inputs and expected outputs and run them against the new code, with a skip list that shrinks to zero.
- When several implementations of a standard exist, have them define it: build a conformance suite that all of them pass, then run the new code against it.

## Rules that must always hold

- State the property that must always hold in one sentence, for example "a customer is charged exactly once" or "every queued message is delivered or reported as failed".
- Generate sequences of operations with a property-based testing library, and check the property after every step.
- Run the same sequences while injecting faults, using the techniques in Failures and timing. A property that holds only on the happy path is not proven.
- For a distributed system, check the property under network partitions, clock jumps, and crashed containers. A deterministic simulation platform, such as Antithesis, runs the real containers, injects these faults, and replays the exact schedule that broke the property.
- Make failures replayable: fix and record the random seed, or run under a deterministic simulator, so that a failing sequence can be rerun on both sides.

## Behavior that varies between runs

When the outcome varies between runs, for example a feature with a language model inside it, one passing run proves little.

- Run each scenario many times on both sides with identical inputs. Choose the count and the passing rate before running.
- Report pass rates per scenario, such as 3 of 10 before and 10 of 10 after, and read the failing transcripts to tell a real failure from an acceptable variation.
- When success is a judgment, such as whether an answer satisfied the user, report the share of runs that succeeded across all scenarios rather than a pass or fail.
- Pin what can be pinned, such as seeds, temperature, and recorded tool results, so the remaining variation comes from the part under test.

## Simulated users

Scripted cases cover only the paths someone thought of.

- Have agents act as users: spawn Engineers, give each a persona and a goal, let it drive the real interface or the stack backed by fakes, and record what it did. Many simulated users find paths that no scripted case covers.
- Include adversarial users who try to misuse the feature: steps in the wrong order, invalid input, crossing permission boundaries, and pushing past limits.
- Judge each run against the goal, and report the share of runs that succeeded, as in Behavior that varies between runs.

## Observing what the user cannot see

- Read the persisted result, such as the stored tool output, the database row, or the commit, rather than a UI's or an agent's report of it.
- Inspect a launched process's real arguments and environment, for example through `/proc/<pid>/cmdline` and `/proc/<pid>/environ` on Linux.
- Count resources before, during, and after: open file descriptors, file watches, sockets, and child processes.
- Record network traffic as a HAR file to count requests, bytes, and repeated fetches.
- For a terminal UI, capture the pane with its colors and render it, or run a real terminal emulator and record its window.
- For animation, record at a high frame rate and inspect every frame for jumps and jitter.
- When a change must prevent something, assert its absence: no mail in the sink, no row written, no process spawned.
