use super::*;

#[test]
fn top_level_pr_init_parses() {
    let args = Args::try_parse(["pr".to_owned(), "init".to_owned()].into_iter()).unwrap();
    assert!(matches!(
        args.command,
        super::Command::Pr(super::PrArgs {
            command: super::PrCliCommand::Init,
            ..
        })
    ));
}

#[test]
fn pr_comment_parses_with_optional_reply() {
    let args = Args::try_parse(
        [
            "pr",
            "comment",
            "https://github.com/acme/widgets/pull/1",
            "--reply",
            "inline:9:v1",
            "--body",
            "addressed",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    assert!(matches!(
        args.command,
        super::Command::Pr(super::PrArgs {
            command: super::PrCliCommand::Comment {
                url,
                reply: Some(reply),
                body,
            },
            ..
        }) if url == "https://github.com/acme/widgets/pull/1"
            && reply == "inline:9:v1"
            && body == "addressed"
    ));
}

#[test]
fn bare_rho_requires_a_subcommand() {
    assert!(Args::try_parse(std::iter::empty()).is_err());
}
