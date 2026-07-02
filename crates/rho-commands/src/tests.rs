use std::str::FromStr as _;

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
            working_directory: Some(PathBuf::from("~/src/rho"))
        }))
    );
    assert_eq!(
        parse(":agent load agent-3"),
        Some(Parsed::Command(Command::AgentLoad {
            agent_id: AgentId::from_str("agent-3").unwrap()
        }))
    );
    assert_eq!(
        parse(":agent load"),
        Some(Parsed::Invalid(":agent load <agent-id>".to_owned()))
    );
    assert_eq!(parse(":cancel"), Some(Parsed::Command(Command::AgentCancel)));
}

#[test]
fn parses_topic_new_with_multi_word_name() {
    assert_eq!(
        parse(":topic new fix auth bug"),
        Some(Parsed::Command(Command::TopicNew {
            name: Some("fix auth bug".to_owned())
        }))
    );
    assert_eq!(
        parse(":topic new"),
        Some(Parsed::Command(Command::TopicNew { name: None }))
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
fn resolves_topics_by_label_or_id() {
    let topics = vec![
        ("infra".to_owned(), TopicId::from_str("topic-2").unwrap()),
        ("topic-1".to_owned(), TopicId::from_str("topic-1").unwrap()),
    ];
    assert_eq!(resolve_topic("infra", &topics), Some(topics[0].1));
    assert_eq!(resolve_topic("topic-2", &topics), Some(topics[0].1));
    assert_eq!(resolve_topic("new-topic", &topics), None);
}

#[test]
fn parses_workdir_commands() {
    assert_eq!(
        parse(":workdirs add /home/u/src/rho rho"),
        Some(Parsed::Command(Command::WorkdirAdd {
            path: Some(PathBuf::from("/home/u/src/rho")),
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
    assert_eq!(
        parse(":wat"),
        Some(Parsed::Unknown(":wat".to_owned()))
    );
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
    assert_eq!(values(&second), ["new", "load", "cancel"]);

    let partial = completion_candidates(":agent lo", &ctx);
    assert_eq!(values(&partial), ["load"]);
}

#[test]
fn completes_arguments_from_context() {
    let workdirs = vec![("rho".to_owned(), "/home/u/src/rho".to_owned())];
    let agents = vec!["agent-1".to_owned(), "agent-2".to_owned()];
    let topics = vec!["infra".to_owned(), "topic-1".to_owned()];
    let ctx = CompletionCtx {
        workdirs: &workdirs,
        known_agents: &agents,
        topics: &topics,
    };

    assert_eq!(values(&completion_candidates(":agent new ", &ctx)), ["rho"]);
    assert_eq!(
        values(&completion_candidates(":topic move in", &ctx)),
        ["infra"]
    );
    assert_eq!(
        values(&completion_candidates(":agent load 2", &ctx)),
        ["agent-2"]
    );
    assert_eq!(values(&completion_candidates(":workdirs rm rh", &ctx)), ["rho"]);
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
