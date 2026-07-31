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
fn pr_comment_parses() {
    let args = Args::try_parse(
        [
            "pr",
            "comment",
            "https://github.com/acme/widgets/pull/1",
            "--reply-comment",
            "7",
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
                reply_comment: Some(reply_comment),
                body,
            },
            ..
        }) if url == "https://github.com/acme/widgets/pull/1"
            && reply_comment == 7
            && body == "addressed"
    ));
}

#[test]
fn pr_comments_parses() {
    let args = Args::try_parse(
        ["pr", "comments", "https://github.com/acme/widgets/pull/1"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    assert!(matches!(
        args.command,
        super::Command::Pr(super::PrArgs {
            command: super::PrCliCommand::Comments { url },
            ..
        }) if url == "https://github.com/acme/widgets/pull/1"
    ));
}

#[test]
fn pr_edit_parses_title_and_description() {
    let args = Args::try_parse(
        [
            "pr",
            "edit",
            "https://github.com/acme/widgets/pull/1",
            "--base",
            "release",
            "--title",
            "Better title",
            "--description",
            "Better summary",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    assert!(matches!(
        args.command,
        super::Command::Pr(super::PrArgs {
            command: super::PrCliCommand::Edit {
                url,
                base: Some(base),
                title: Some(title),
                body: Some(body),
            },
            ..
        }) if url == "https://github.com/acme/widgets/pull/1"
            && base == "release"
            && title == "Better title"
            && body == "Better summary"
    ));
}

#[test]
fn pr_checks_parses_watch_interval() {
    let args = Args::try_parse(
        [
            "pr",
            "checks",
            "https://github.com/acme/widgets/pull/1",
            "--watch",
            "--interval",
            "5",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    assert!(matches!(
        args.command,
        super::Command::Pr(super::PrArgs {
            command: super::PrCliCommand::Checks {
                url,
                watch: true,
                interval: 5,
            },
            ..
        }) if url == "https://github.com/acme/widgets/pull/1"
    ));
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
fn ws_alias_parses_to_workstream_commands() {
    let args = Args::try_parse(["ws".to_owned(), "list".to_owned()].into_iter()).unwrap();
    assert!(matches!(
        args.command,
        super::Command::Workstream(super::WorkstreamArgs {
            command: super::WorkstreamCommand::List,
            ..
        })
    ));

    let args = Args::try_parse(
        ["workstream", "move", "eng-16lh", "gui rebuild"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    assert!(matches!(
        args.command,
        super::Command::Workstream(super::WorkstreamArgs {
            command: super::WorkstreamCommand::Move { agent, workstream },
            ..
        }) if agent == "eng-16lh" && workstream == "gui rebuild"
    ));
}
