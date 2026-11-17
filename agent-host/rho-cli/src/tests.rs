use super::*;

/// Lines come by account and then window, with the used share rather than
/// the remaining one; ChatGPT quota and unknown resets do not break them.
#[test]
fn claude_usage_lines_by_account_and_window() {
    let summary = |model: &str, account: Option<&str>, remaining: u8, reset: Option<i64>| {
        agents::QuotaSummary {
            model: model.to_owned(),
            auth_namespace: account.map(str::to_owned),
            remaining_percent: remaining,
            burn_10m: 0,
            burn_2h: 0,
            burn_1d: 0,
            burn_3d: 0,
            reset_at_unix: reset,
        }
    };
    let lines = claude_usage_lines(&[
        summary("fable", Some("work"), 88, Some(1_786_000_000)),
        summary("gpt", Some("chatgpt"), 50, Some(1_786_000_000)),
        summary("opus", Some("work"), 93, Some(1_786_000_000)),
        summary("opus", Some("default"), 60, None),
        summary("session", Some("work"), 77, Some(1_785_600_000)),
    ]);
    assert_eq!(
        lines,
        vec![
            "default\tweekly\t40%\t-",
            "work\t5-hour\t23%\t2026-08-01T16:00Z",
            "work\tweekly\t7%\t2026-08-06T07:06Z",
            "work\tweekly-fable\t12%\t2026-08-06T07:06Z",
        ]
    );
}

#[test]
fn github_init_parses() {
    let args = Args::try_parse(["github", "init"].into_iter().map(str::to_owned)).unwrap();
    assert!(matches!(
        args.command,
        Command::Github(GithubArgs {
            command: GithubCommand::Init,
            ..
        })
    ));
}

#[test]
fn pr_commands_are_not_available() {
    assert!(Args::try_parse(["pr", "status"].into_iter().map(str::to_owned)).is_err());
}

/// A bare `rho` is `rho share`; `attach` and the old chat's flags are
/// gone with the chat.
#[test]
fn bare_rho_is_share() {
    let parse = |args: &[&str]| Args::try_parse(args.iter().map(|arg| (*arg).to_owned()));
    assert!(matches!(parse(&[]).unwrap().command, Command::Share(_)));
    assert!(matches!(
        parse(&["share"]).unwrap().command,
        Command::Share(_)
    ));
    assert!(parse(&["attach", "abc"]).is_err());
    assert!(parse(&["--role", "high-eng"]).is_err());
}

#[test]
fn record_visualization_parses() {
    let args = Args::try_parse(["record-visualization".to_owned()].into_iter()).unwrap();
    assert!(matches!(
        args.command,
        super::Command::RecordVisualization(_)
    ));
}

#[test]
fn evaluation_accepts_sol_role() {
    let args = Args::try_parse(
        ["eval", "task", "--role", "med-eng"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    let Command::Eval(eval) = args.command else {
        panic!("not an eval");
    };
    assert_eq!(eval.role, "med-eng");
}

#[test]
fn evaluation_defaults_to_astra_and_requires_a_prompt() {
    assert!(Args::try_parse(["eval".to_owned()].into_iter()).is_err());
    let args = Args::try_parse(
        [
            "eval",
            "smoke test",
            "--expect",
            "PASS",
            "--require-tool",
            "exec",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    let Command::Eval(eval) = args.command else {
        panic!("not an eval");
    };
    assert_eq!(eval.role, "high-eng");
    assert_eq!(eval.expect, ["PASS"]);
    assert_eq!(eval.require_tool, ["exec"]);
    assert!(
        Args::try_parse(
            ["eval", "task", "--timeout", "0"]
                .into_iter()
                .map(str::to_owned)
        )
        .is_err()
    );
}

#[test]
fn reset_listing_and_redemption_parse() {
    let parse = |args: &[&str]| Args::try_parse(args.iter().map(|s| s.to_string()));
    assert!(matches!(
        parse(&["auth", "resets"]).unwrap().command,
        Command::Auth(AuthArgs::Resets { name }) if name == "default"
    ));
    assert!(matches!(
        parse(&["auth", "resets", "--name", "second"]).unwrap().command,
        Command::Auth(AuthArgs::Resets { name }) if name == "second"
    ));
    assert!(parse(&["auth", "reset"]).is_err());
    assert!(matches!(
        parse(&["auth", "reset", "--name", "second", "--credit-id", "credit-2",
                "--idempotency-key", "retry-key"]).unwrap().command,
        Command::Auth(AuthArgs::Reset { name, credit_id: Some(id), idempotency_key: Some(key) })
            if name == "second" && id == "credit-2" && key == "retry-key"
    ));
    assert!(matches!(
        parse(&["auth", "reset", "--name", "default"]).unwrap().command,
        Command::Auth(AuthArgs::Reset { name, credit_id: None, idempotency_key: None })
            if name == "default"
    ));
}
