### Basic usage

```sh
# Check for violations (exit 1 on failure)
codestyle rust assert .

# Auto-fix violations in place
codestyle rust format .

# Collect occurrences into a per-rule markdown worktable for manual review
codestyle rust --only ignored-error audit .
```

### Toggling checks

Each check has a default and can be flipped with `--<check>=true|false`. Pass flags before the subcommand:

```sh
# Enable instrument check (off by default)
codestyle rust --instrument=true assert .

# Disable specific checks
codestyle rust --loops=false --embed-simple-vars=false assert .
```

### Excluding paths

Use `--exclude` (repeatable, before the subcommand) to skip vendored or third-party trees:

```sh
codestyle --exclude libs/nautilus_trader --exclude vendor rust assert .
```

### Available flags

| Flag | Default | Description |
|------|---------|-------------|
| `--cargo-dep-ordering` | true | Order and group dependencies in `Cargo.toml` |
| `--instrument` | false | Require `#[instrument]` on async functions |
| `--loops` | true | Endless loops must carry a `//LOOP` comment |
| `--join-split-impls` | true | Join split `impl` blocks for the same type |
| `--impl-folds` | false | Wrap `impl` blocks in vim 1-fold markers |
| `--impl-follows-type` | true | `impl` blocks follow their type definition |
| `--embed-simple-vars` | true | Embed simple vars directly in format strings |
| `--insta-inline-snapshot` | true | `insta` macros use inline `@""` syntax |
| `--no-chrono` | true | Forbid `chrono` (use `jiff` instead) |
| `--no-tokio-spawn` | true | Forbid `tokio::spawn` (use structured concurrency) |
| `--use-bail` | true | Replace `return Err(eyre!(...))` with `bail!(...)` |
| `--test-fn-prefix` | false | Test fns must not start with `test_` |
| `--pub-first` | true | `pub` items come before private items |
| `--ignored-error` | true | `unwrap_or*` and `let _ =` need `//IGNORED_ERROR` |
| `--workspace-dep-hoisting` | true | Hoist shared deps to `[workspace.dependencies]` |
| `--unconventional-new` | true | `fn new` returning `Result` -> rename to `try_new` |
| `--prefer-default-over-bare-new` | false | Argument-less `pub fn new()` -> `Default` |
| `--inline-default` | true | Inline `impl Default` bodies as field defaults (RFC 3681) |
| `--prefer-ahash` | false | Replace `HashMap` with `ahash::AHashMap` |
| `--too-explicit` | true | Rewrite inline fully-qualified `std::` paths and add imports |

### Format mode

`format` will:
1. Auto-fix violations where possible.
2. Delete `.snap` / `.pending-snap` files (when the `insta` check is enabled).
3. Report violations that still need manual fixing.

```sh
codestyle rust format .
# codestyle: fixed 3 violation(s)
# codestyle: 1 violation(s) need manual fixing:
#   [loops] src/main.rs:42:5: Endless loop without //LOOP comment
```

### Audit mode

Some rules flag patterns that are genuinely hard to auto-fix and need human judgement case-by-case
(e.g. `ignored-error` flags every `unwrap_or*` / `let _ = …` — each must be individually decided
"keep & justify vs switch to Error/Panic"). `audit` scaffolds that review: it collects every
occurrence of each audit-capable rule into `<target_dir>/tmp/audit/<rule>.md` (override with
`--audit-dir`) as a `- [ ]` checklist with a `TODO: reason` line per item, under a header that
spells out the default decision. It's a collection step, not a gate — it always exits 0 on success.

Only a subset of rules know how to audit (currently just `ignored-error`), and audit is normally
run with `--only`:

```sh
codestyle rust --only ignored-error audit .
# codestyle: wrote 12 occurrence(s) to ./tmp/audit/ignored-error.md
```
