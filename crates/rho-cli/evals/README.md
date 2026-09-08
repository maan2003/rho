# Headless agent evaluations

`rho eval` runs the real agent loop and the selected role's tool surface in-process,
using configured provider credentials but a temporary database and (by default)
an empty temporary working directory. It does not connect to or restart the
running daemon. `eng-high` is GPT-6 Astra, using the same role profile as the GUI.
These evaluations make paid/provider-metered requests.
Among eval roles, only `eng-high` currently defaults to Python; `eng`, `eng-cheap`, and `eng-low`
use JavaScript. Earlier cross-model Python transcripts predate this default change.

```sh
cargo run -p rho-cli -- eval --role eng-high --workdir /path/to/task-checkout \
  --prompt-file task.txt --require-tool exec --timeout 600 > coding-eval.jsonl
```

Output is JSONL: request boundaries, assistant text, actual tool calls/results
and updates, usage, then a summary. Provider reasoning and image bytes are not
included. Keep the transcript on disk; inspect just the summary:

```sh
python3 - <<'PY'
import json
with open('coding-eval.jsonl') as log:
    for line in log:
        record = json.loads(line)
        if record['type'] == 'summary':
            print(json.dumps(record, indent=2))
PY
```

The timeout covers agent execution, not setup or a blocked output sink.
A provider failure, timeout, interruption, missing `--expect` substring in the
final response, or missing actual `--require-tool` call exits nonzero. Assertions
are repeatable. These assertions check execution and model-reported results,
not patch correctness. Grade the resulting checkout independently.

For another evaluation, pass task text positionally or use `--prompt-file -`
for stdin. `--workdir PATH` opts into a **live** existing checkout/directory: tool
writes there are real and are not reverted. Without it, workspace fixtures and
evaluation state are discarded on exit. Transcript output can contain task and
file contents; treat it like any other Rho transcript.

## Evaluate real coding work, not notebook compliance

Use ordinary task instructions and external tests. [The pytest chained-exception
evaluation](pytest-chains.md) uses a real historical repository bug to exercise
the tool during ordinary work. Inspect command usage, API errors, filesystem
operations, and output handling—not model rankings. Independent tests provide
an additional check that the resulting edits are usable.

`python-smoke.txt` is only a notebook plumbing check, not a coding benchmark.

## Small diagnostic fixture (independently graded)

`retry-report.txt` is a normal bug-fix request with no code-mode instructions.
Give each run a fresh copy of `retry-report/`, pass it with `--workdir`, and grade
the resulting implementation using `grade_retry_report.py` **outside** the
agent's workdir. The grader checks ordering, retries, ties, unfinished attempts,
missing durations, iterable inputs, and input preservation, including randomized
cases. A model's claim that its tests pass is not the grading result.

```sh
python3 - <<'PY'
import pathlib, shutil, subprocess, tempfile
fixture = pathlib.Path('crates/rho-cli/evals')
with tempfile.TemporaryDirectory() as work:
    shutil.copytree(fixture / 'retry-report', work, dirs_exist_ok=True)
    with open('coding-eval.jsonl', 'w') as log:
        result = subprocess.run([
            'target/debug/rho', 'eval', '--role', 'eng-high', '--workdir', work,
            '--prompt-file', str(fixture / 'retry-report.txt'),
            '--require-tool', 'exec', '--timeout', '300',
        ], stdout=log)
    grade = subprocess.run(['python3', str(fixture / 'grade_retry_report.py'), work])
    raise SystemExit(result.returncode or grade.returncode)
PY
```

Change the role to `eng` for Sol or `eng-cheap` for Terra/high reasoning.
