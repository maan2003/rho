# Real-repository evaluation: pytest exception serialization

Task: fix report serialization losing implicit and explicit exception chains,
so distributed pytest runs display the same chained traceback as local runs.
This exercises traceback representations, JSON serialization/deserialization,
test and collection reports, and regression coverage—not notebook compliance.
The goal is tool usability, not model performance. Observe redundant awaits,
output access, direct file operations, and API/runtime errors. Upstream research
is ordinary agent work, not a failure of this evaluation.

## Reproduce

- Dataset: [SWE-bench Verified](https://huggingface.co/datasets/princeton-nlp/SWE-bench_Verified),
  instance `pytest-dev__pytest-5787`.
- Repository: `pytest-dev/pytest`, base commit
  `955e54221008aba577ecbaefa15679f6777d3bf8`.
- Give each model a fresh checkout. Keep dataset `patch` and `test_patch`
  outside the agent workdir. The task text is the row's `problem_statement`,
  prefixed with “Fix the following pytest bug. Add regression coverage and run
  the relevant tests.” Supply the prepared test command below.
- Run `rho eval --role ROLE --workdir CHECKOUT --prompt-file TASK --timeout 600
  --require-tool exec`, saving JSONL separately for each role.

Use an isolated Python 3.9 environment. The tested versions were Python 3.9.25,
pytest 5.1.0, py 1.11.0, attrs 23.2.0, packaging 23.2, more-itertools 8.14.0,
pluggy 0.13.1, hypothesis 5.49.0, setuptools 69.5.1, setuptools-scm 7.1.0,
pytest-xdist 1.34.0, plus atomicwrites, wcwidth, mock, nose, and requests.
For source archives without VCS metadata, write `version = "5.1.0"` to
`src/_pytest/_version.py`. Use the environment's absolute Python path:

```sh
PYTHONPATH=src PYTEST_DISABLE_PLUGIN_AUTOLOAD=1 /path/to/venv/bin/python -m pytest
```

## Grade independently

In another clean baseline copy, apply only the dataset `test_patch`. Overlay
the candidate's `src/` without bytecode caches; do not copy its modified tests.
Run these three test files with the command above:

```text
testing/test_reports.py
testing/code/test_code.py
testing/code/test_excinfo.py
```

Validate the harness first: the baseline fails both parametrizations of
`TestReportSerialization::test_chained_exceptions` (TestReport and CollectReport),
with 123 passes. Applying the known production patch gives 125 passes.
Both runs skip two tests needing optional decorator/jinja2 dependencies.

These tests check the resulting artifact, not whether the model solved the issue
independently. They do not replace examining the tool interaction itself.
