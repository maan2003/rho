use camino::Utf8PathBuf;

use super::*;

fn values(candidates: &[Candidate]) -> Vec<&str> {
    candidates
        .iter()
        .map(|candidate| candidate.value.as_str())
        .collect()
}

#[test]
fn non_colon_lines_are_messages() {
    assert_eq!(parse("hello world"), None);
    assert_eq!(parse("/quit"), None);
}

#[test]
fn parses_agent_commands() {
    assert_eq!(
        parse(":agent new"),
        Some(Parsed::Command(Command::AgentNew {
            working_directory: None
        }))
    );
    assert_eq!(
        parse(":agent new ~/src/rho"),
        Some(Parsed::Command(Command::AgentNew {
            working_directory: Some(Utf8PathBuf::from("~/src/rho"))
        }))
    );
    assert_eq!(
        parse(":cancel"),
        Some(Parsed::Command(Command::AgentCancel))
    );
    assert_eq!(
        parse(":agent rename build fixer"),
        Some(Parsed::Command(Command::AgentRename {
            name: "build fixer".to_owned()
        }))
    );
    assert_eq!(
        parse(":agent rename"),
        Some(Parsed::Invalid(":agent rename <name>".to_owned()))
    );
}

#[test]
fn parses_agent_mode_commands() {
    assert_eq!(
        parse(":agent fast"),
        Some(Parsed::Command(Command::AgentFast { enabled: None }))
    );
    assert_eq!(
        parse(":agent fast off"),
        Some(Parsed::Command(Command::AgentFast {
            enabled: Some(false)
        }))
    );
    assert_eq!(
        parse(":agent effort xhigh"),
        Some(Parsed::Command(Command::AgentEffort {
            effort: DeepEffort::Xhigh
        }))
    );
    assert_eq!(
        parse(":agent effort"),
        Some(Parsed::Invalid(
            ":agent effort <low|medium|xhigh>".to_owned()
        ))
    );
}

#[test]
fn parses_topic_new_with_multi_word_name() {
    assert_eq!(
        parse(":topic new fix auth bug"),
        Some(Parsed::Command(Command::TopicNew {
            name: "fix auth bug".to_owned()
        }))
    );
    // Unnamed topics don't exist; the name is required.
    assert_eq!(
        parse(":topic new"),
        Some(Parsed::Invalid(":topic new <name>".to_owned()))
    );
}

#[test]
fn parses_topic_move() {
    assert_eq!(
        parse(":topic move fix auth bug"),
        Some(Parsed::Command(Command::TopicMove {
            name: "fix auth bug".to_owned()
        }))
    );
    assert_eq!(
        parse(":topic move"),
        Some(Parsed::Invalid(":topic move <name>".to_owned()))
    );
}

#[test]
fn parses_topic_rename() {
    assert_eq!(
        parse(":topic rename fix auth bug"),
        Some(Parsed::Command(Command::TopicRename {
            name: "fix auth bug".to_owned()
        }))
    );
    assert_eq!(
        parse(":topic rename"),
        Some(Parsed::Invalid(":topic rename <name>".to_owned()))
    );
}

#[test]
fn resolves_topics_by_label() {
    let domain = rho_ui_proto::TopicIdDomain(0);
    let topics = vec![
        (
            "infra".to_owned(),
            TopicId::from_counter(2, &domain).unwrap(),
        ),
        ("1".to_owned(), TopicId::from_counter(1, &domain).unwrap()),
    ];
    assert_eq!(resolve_topic("infra", &topics), Some(topics[0].1));
    assert_eq!(resolve_topic("1", &topics), Some(topics[1].1));
    assert_eq!(resolve_topic("new-topic", &topics), None);
}

#[test]
fn parses_status_commands() {
    assert_eq!(
        parse(":agent pin"),
        Some(Parsed::Command(Command::AgentPin))
    );
    assert_eq!(
        parse(":agent archive"),
        Some(Parsed::Command(Command::AgentArchive))
    );
    assert_eq!(
        parse(":topic pin"),
        Some(Parsed::Command(Command::TopicPin { name: None }))
    );
    assert_eq!(
        parse(":topic archive old work"),
        Some(Parsed::Command(Command::TopicArchive {
            name: Some("old work".to_owned())
        }))
    );
}

#[test]
fn toggle_status_round_trips() {
    assert_eq!(
        toggle_status(Status::Normal, Status::Pinned),
        Status::Pinned
    );
    assert_eq!(
        toggle_status(Status::Pinned, Status::Pinned),
        Status::Normal
    );
    // Pinning an archived item surfaces it.
    assert_eq!(
        toggle_status(Status::Archived, Status::Pinned),
        Status::Pinned
    );
    assert_eq!(
        toggle_status(Status::Archived, Status::Archived),
        Status::Normal
    );
}

#[test]
fn parses_workdir_commands() {
    assert_eq!(
        parse(":workdirs add /home/u/src/rho rho"),
        Some(Parsed::Command(Command::WorkdirAdd {
            path: Some(Utf8PathBuf::from("/home/u/src/rho")),
            name: Some("rho".to_owned()),
        }))
    );
    assert_eq!(
        parse(":workdirs add"),
        Some(Parsed::Command(Command::WorkdirAdd {
            path: None,
            name: None
        }))
    );
    assert_eq!(
        parse(":workdirs rm rho"),
        Some(Parsed::Command(Command::WorkdirRemove {
            path: "rho".to_owned()
        }))
    );
}

#[test]
fn unknown_commands_are_reported() {
    assert_eq!(parse(":wat"), Some(Parsed::Unknown(":wat".to_owned())));
}

#[test]
fn completes_command_words_stepwise() {
    let ctx = CompletionCtx::default();
    let first = completion_candidates(":", &ctx);
    assert!(values(&first).contains(&"agent"));
    assert!(values(&first).contains(&"workdirs"));
    // Group words appear once, not per subcommand.
    assert_eq!(values(&first).iter().filter(|v| **v == "agent").count(), 1);

    let second = completion_candidates(":agent ", &ctx);
    assert_eq!(
        values(&second),
        [
            "new", "cancel", "rename", "pin", "archive", "fast", "effort"
        ]
    );

    let partial = completion_candidates(":agent ar", &ctx);
    assert_eq!(values(&partial), ["archive"]);
}

#[test]
fn completes_arguments_from_context() {
    let workdirs = vec![("rho".to_owned(), "/home/u/src/rho".to_owned())];
    let topics = vec!["infra".to_owned(), "1".to_owned()];
    let ctx = CompletionCtx {
        workdirs: &workdirs,
        topics: &topics,
    };

    assert_eq!(values(&completion_candidates(":agent new ", &ctx)), ["rho"]);
    assert_eq!(
        values(&completion_candidates(":agent fast ", &ctx)),
        ["on", "off"]
    );
    assert_eq!(
        values(&completion_candidates(":agent effort x", &ctx)),
        ["xhigh"]
    );
    assert_eq!(
        values(&completion_candidates(":topic move in", &ctx)),
        ["infra"]
    );
    assert_eq!(
        values(&completion_candidates(":workdirs rm rh", &ctx)),
        ["rho"]
    );
    // Paths for `workdirs add` come from the client's filesystem completion.
    assert_eq!(completion_candidates(":workdirs add ", &ctx), Vec::new());
}

#[test]
fn every_command_name_parses() {
    for spec in COMMANDS {
        let line = format!(":{}", spec.name);
        match parse(&line) {
            None | Some(Parsed::Unknown(_)) => {
                panic!("`{}` completes but does not dispatch", spec.name)
            }
            // Commands with required arguments report usage on bare
            // invocation, which is still dispatch.
            Some(Parsed::Invalid(_) | Parsed::Command(_)) => {}
        }
    }
}

#[test]
fn resolves_workdirs_by_name_or_path() {
    let workdirs = vec![("rho".to_owned(), "/home/u/src/rho".to_owned())];
    assert_eq!(resolve_workdir("rho", &workdirs), Some("/home/u/src/rho"));
    assert_eq!(
        resolve_workdir("/home/u/src/rho", &workdirs),
        Some("/home/u/src/rho")
    );
    assert_eq!(resolve_workdir("zed", &workdirs), None);
}
