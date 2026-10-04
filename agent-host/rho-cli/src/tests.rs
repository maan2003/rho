use super::*;

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

#[test]
fn bare_rho_requires_a_subcommand() {
    assert!(Args::try_parse(std::iter::empty()).is_err());
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
    assert!(matches!(
        args.command, Command::Eval(eval::EvalArgs { role, .. }) if role == "med-eng"
    ));
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
    assert!(
        matches!(args.command, Command::Eval(eval::EvalArgs {role, expect, require_tool,..}) if role == "high-eng" && expect == ["PASS"] && require_tool == ["exec"])
    );
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
