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
    assert_eq!(parse(":continue"), Some(Parsed::Command(Command::Continue)));
    assert_eq!(parse(":compact"), Some(Parsed::Command(Command::Compact)));
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
    assert_eq!(
        parse(":agent change-prompt-cache-key"),
        Some(Parsed::Command(Command::AgentChangePromptCacheKey))
    );
    assert_eq!(
        parse(":rewind"),
        Some(Parsed::Command(Command::Rewind { turns: 1 }))
    );
    assert_eq!(
        parse(":rewind 3"),
        Some(Parsed::Command(Command::Rewind { turns: 3 }))
    );
    assert_eq!(
        parse(":rewind 0"),
        Some(Parsed::Invalid(":rewind [turns]".to_owned()))
    );
}

#[test]
fn parses_tag_move_with_multi_word_name() {
    assert_eq!(
        parse(":tag move fix auth bug"),
        Some(Parsed::Command(Command::TagMove {
            name: "fix auth bug".to_owned()
        }))
    );
    assert_eq!(
        parse(":tag move"),
        Some(Parsed::Invalid(":tag move <workstream>".to_owned()))
    );
}

#[test]
fn parses_tag_group_and_labels() {
    assert_eq!(
        parse(":tag group infra work"),
        Some(Parsed::Command(Command::TagGroup {
            name: "infra work".to_owned()
        }))
    );
    assert_eq!(
        parse(":tag label urgent"),
        Some(Parsed::Command(Command::TagLabel {
            name: "urgent".to_owned()
        }))
    );
    assert_eq!(
        parse(":tag unlabel urgent"),
        Some(Parsed::Command(Command::TagUnlabel {
            name: "urgent".to_owned()
        }))
    );
}

#[test]
fn parses_tag_rename() {
    assert_eq!(
        parse(":tag rename fix auth bug"),
        Some(Parsed::Command(Command::TagRename {
            name: "fix auth bug".to_owned()
        }))
    );
    assert_eq!(
        parse(":tag rename"),
        Some(Parsed::Invalid(":tag rename <name>".to_owned()))
    );
}

#[test]
fn resolves_tags_by_name() {
    let tags = vec![
        ("infra".to_owned(), TagId(2)),
        ("1".to_owned(), TagId(1)),
    ];
    assert_eq!(resolve_tag("infra", &tags), Some(TagId(2)));
    assert_eq!(resolve_tag("1", &tags), Some(TagId(1)));
    assert_eq!(resolve_tag("new-tag", &tags), None);
}

#[test]
fn parses_status_commands() {
    assert_eq!(
        parse(":agent pin"),
        Some(Parsed::Command(Command::AgentPin))
    );
    assert_eq!(
        parse(":tag pin"),
        Some(Parsed::Command(Command::TagPin { name: None }))
    );
    assert_eq!(
        parse(":done"),
        Some(Parsed::Command(Command::AgentDone { hide: false }))
    );
    assert_eq!(
        parse(":done hide"),
        Some(Parsed::Command(Command::AgentDone { hide: true }))
    );
    assert!(matches!(parse(":done later"), Some(Parsed::Invalid(_))));
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
}

#[test]
fn parses_project_commands() {
    assert_eq!(
        parse(":projects add /home/u/src/rho rho"),
        Some(Parsed::Command(Command::ProjectAdd {
            path: Some(Utf8PathBuf::from("/home/u/src/rho")),
            name: Some("rho".to_owned()),
            description: String::new(),
        }))
    );
    assert_eq!(
        parse(":projects add"),
        Some(Parsed::Command(Command::ProjectAdd {
            path: None,
            name: None,
            description: String::new(),
        }))
    );
    assert_eq!(
        parse(":projects rm rho"),
        Some(Parsed::Command(Command::ProjectRemove {
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
    assert!(values(&first).contains(&"projects"));
    // Group words appear once, not per subcommand.
    assert_eq!(values(&first).iter().filter(|v| **v == "agent").count(), 1);

    let second = completion_candidates(":agent ", &ctx);
    assert_eq!(
        values(&second),
        ["new", "cancel", "rename", "change-prompt-cache-key", "pin"]
    );

    let partial = completion_candidates(":agent re", &ctx);
    assert_eq!(values(&partial), ["rename"]);
}

#[test]
fn completes_arguments_from_context() {
    let workdirs = vec![("rho".to_owned(), "/home/u/src/rho".to_owned())];
    let workstreams = vec!["infra".to_owned(), "1".to_owned()];
    let groups = vec!["backend".to_owned()];
    let labels = vec!["urgent".to_owned()];
    let ctx = CompletionCtx {
        workdirs: &workdirs,
        workstreams: &workstreams,
        groups: &groups,
        labels: &labels,
    };

    assert_eq!(values(&completion_candidates(":agent new ", &ctx)), ["rho"]);
    assert_eq!(
        values(&completion_candidates(":tag move in", &ctx)),
        ["infra"]
    );
    assert_eq!(
        values(&completion_candidates(":tag group ba", &ctx)),
        ["backend"]
    );
    assert_eq!(
        values(&completion_candidates(":tag unlabel ", &ctx)),
        ["urgent"]
    );
    assert_eq!(
        values(&completion_candidates(":projects rm rh", &ctx)),
        ["rho"]
    );
    // Paths for `workdirs add` come from the client's filesystem completion.
    assert_eq!(completion_candidates(":projects add ", &ctx), Vec::new());
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
