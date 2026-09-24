// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ci.sh scripts drive rho's own in-script client when it is on PATH.

use std::path::Path;

use rho_ci::config::JobConfig;
use rho_ci::{Candidate, CheckOptions, protocol, run_check};

fn job(command: &str) -> JobConfig {
    rho_ci::config::parse_config(&format!("job:\n  command: {command}\n"))
        .unwrap()
        .job
}

fn selfci_path_prefix() -> String {
    Path::new(env!("CARGO_BIN_EXE_selfci"))
        .parent()
        .unwrap()
        .display()
        .to_string()
}

fn run(job: &JobConfig, work: &Path) -> rho_ci::CheckResult {
    run_check(
        CheckOptions {
            job,
            candidate_dir: work,
            candidate: Candidate {
                commit_id: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                change_id: "zzzzzzzzzzzzzzzz".to_owned(),
                display: "test-candidate".to_owned(),
            },
        },
        |_| {},
    )
    .unwrap()
}

#[test]
fn path_client_reports_steps_and_jobs() {
    let work = tempfile::tempdir().unwrap();
    let path = selfci_path_prefix();
    let job = job(&format!(
        r#"|
    export PATH="{path}:$PATH"
    if [ "$SELFCI_JOB_NAME" = main ]; then
      selfci job start second
      selfci step start build
      true
      selfci step start test
      selfci step fail
      selfci job wait second --success
    else
      selfci step start lint
    fi
    exit 0"#
    ));

    let result = run(&job, work.path());

    // `selfci step fail` on main/test must fail the run even though the
    // script exits 0; `job wait --success` on the green second job must
    // not add a failure of its own.
    assert!(!result.passed, "output: {}", result.output);
    let step = |name: &str| {
        result
            .steps
            .iter()
            .find(|step| step.name == name)
            .unwrap_or_else(|| panic!("missing step {name}; output: {}", result.output))
    };
    assert!(matches!(
        step("main/build").status,
        protocol::StepStatus::Success
    ));
    assert!(matches!(
        step("main/test").status,
        protocol::StepStatus::Failed { ignored: false }
    ));
    assert!(matches!(
        step("second/lint").status,
        protocol::StepStatus::Success
    ));
    assert_eq!(result.jobs.len(), 2);
}

#[test]
fn ignored_step_failure_keeps_run_green() {
    let work = tempfile::tempdir().unwrap();
    let path = selfci_path_prefix();
    let job = job(&format!(
        r#"|
    export PATH="{path}:$PATH"
    selfci step start flaky
    selfci step fail --ignore
    selfci version"#
    ));

    let result = run(&job, work.path());

    assert!(result.passed, "output: {}", result.output);
    assert!(matches!(
        result.steps[0].status,
        protocol::StepStatus::Failed { ignored: true }
    ));
    // `selfci version` output lands in the job's captured output.
    assert!(
        result.output.contains("rho-ci"),
        "output: {}",
        result.output
    );
}
