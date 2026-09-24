// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::Path;

use crate::config::{self, JobConfig, MergeMode};
use crate::{Candidate, CheckEvent, CheckOptions, protocol, run_check};

fn job(command: &str) -> JobConfig {
    config::parse_config(&format!("job:\n  command: {command}\n"))
        .unwrap()
        .job
}

fn candidate() -> Candidate {
    Candidate {
        commit_id: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        change_id: "zzzzzzzzzzzzzzzz".to_owned(),
        display: "test-candidate".to_owned(),
    }
}

fn run(job: &JobConfig, work: &Path, on_event: impl FnMut(CheckEvent)) -> crate::CheckResult {
    run_check(
        CheckOptions {
            job,
            candidate_dir: work,
            candidate: candidate(),
        },
        on_event,
    )
    .unwrap()
}

#[test]
fn parses_selfci_style_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join(".config/selfci");
    std::fs::create_dir_all(&config_dir).unwrap();
    // Mirrors a real repo config: fields rho ignores (clone-mode, hooks)
    // must parse leniently, and the merge-style alias must work.
    std::fs::write(
        config_dir.join("ci.yaml"),
        "job:\n  clone-mode: partial\n  command: ./.config/selfci/ci.sh\n\n\
         mq:\n  base-branch: master\n  merge-style: rebase\n  pre-merge:\n    command: ./notify.sh\n",
    )
    .unwrap();

    let config = config::read_config(dir.path()).unwrap().unwrap();
    assert_eq!(config.job.command, "./.config/selfci/ci.sh");
    assert_eq!(config.job.command_prefix, vec!["bash", "-c"]);

    let mq = config.mq.unwrap();
    assert_eq!(mq.base_branch.as_deref(), Some("master"));
    assert_eq!(mq.merge_mode, MergeMode::Rebase);
}

#[test]
fn missing_config_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(config::read_config(dir.path()).unwrap().is_none());
}

#[test]
fn passing_check_captures_output_and_env() {
    let work = tempfile::tempdir().unwrap();
    let job = job(
        r#"'echo "candidate=$SELFCI_CANDIDATE_COMMIT_ID merged=$SELFCI_MERGED_COMMIT_ID job=$SELFCI_JOB_NAME"; pwd'"#,
    );

    let mut events = Vec::new();
    let result = run(&job, work.path(), |event| events.push(event));

    assert!(result.passed);
    assert!(
        result.output.contains(
            "candidate=0123456789abcdef0123456789abcdef01234567 \
             merged=0123456789abcdef0123456789abcdef01234567 job=main"
        ),
        "unexpected output: {}",
        result.output
    );
    // Jobs run in the candidate checkout.
    let work_canonical = work.path().canonicalize().unwrap();
    assert!(result.output.contains(work_canonical.to_str().unwrap()));
    assert_eq!(result.jobs.len(), 1);
    assert!(matches!(
        result.jobs[0].status,
        protocol::JobStatus::Succeeded
    ));
    assert!(matches!(events[0], CheckEvent::JobStarted { .. }));
    assert!(matches!(
        events.last(),
        Some(CheckEvent::JobFinished { .. })
    ));
}

#[test]
fn failing_check_reports_failure() {
    let work = tempfile::tempdir().unwrap();
    let job = job("'echo boom; exit 3'");

    let result = run(&job, work.path(), |_| {});

    assert!(!result.passed);
    assert!(result.output.contains("boom"));
    assert!(result.output.contains("exit code: 3"));
    assert!(matches!(result.jobs[0].status, protocol::JobStatus::Failed));
}
