use std::io;
use std::path::PathBuf;

use rho::{ProviderResponse, ToolCall, ToolCallId, ToolResult, ToolType};
use rho_cli_term_raw::{CursorShape, Term};
use rho_provider_responses::{ProviderSession, ResponsesUpdate};

use super::*;

fn parse(args: &[&str]) -> Args {
    Args::parse(args.iter().map(|arg| (*arg).to_owned())).expect("args parse")
}

#[test]
fn parses_default_chat_command() {
    let args = parse(&[]);
    let Command::Chat(chat) = args.command else {
        panic!("expected chat command");
    };

    assert_eq!(chat.model, ProviderSession::DEFAULT_MODEL);
    assert_eq!(chat.session, DEFAULT_SESSION_NAME);
    assert!(!chat.prompt_stdin);
    assert!(chat.session_path.is_none());
    assert!(chat.auth_file.ends_with("default.json"));
}

#[test]
fn parses_chat_overrides() {
    let args = parse(&[
        "--model",
        "gpt-test",
        "--auth-file",
        "/tmp/rho-auth.json",
        "--session",
        "work",
        "--session-path",
        "/tmp/rho-session.cbor",
        "--prompt-stdin",
    ]);
    let Command::Chat(chat) = args.command else {
        panic!("expected chat command");
    };

    assert_eq!(chat.model, "gpt-test");
    assert_eq!(chat.session, "work");
    assert_eq!(
        chat.session_path,
        Some(PathBuf::from("/tmp/rho-session.cbor"))
    );
    assert!(chat.prompt_stdin);
    assert_eq!(chat.auth_file, std::path::Path::new("/tmp/rho-auth.json"));
}

#[test]
fn parses_no_store_chat_command() {
    let args = parse(&["--no-store", "--prompt-stdin"]);
    let Command::Chat(chat) = args.command else {
        panic!("expected chat command");
    };

    assert!(chat.no_store);
    assert!(chat.prompt_stdin);
    assert!(chat.session_path.is_none());
}

#[test]
fn no_store_disables_provider_prompt_cache_key() {
    let args = parse(&["--no-store"]);
    let Command::Chat(chat) = args.command else {
        panic!("expected chat command");
    };

    let session = build_provider_session(&chat);

    assert!(session.prompt_cache_key().is_none());
}

#[test]
fn stored_sessions_use_session_name_as_provider_prompt_cache_key() {
    let args = parse(&["--session", "work"]);
    let Command::Chat(chat) = args.command else {
        panic!("expected chat command");
    };

    let session = build_provider_session(&chat);

    assert_eq!(session.prompt_cache_key(), Some("work"));
}

#[test]
fn rejects_no_store_with_explicit_session_path() {
    let error = Args::parse(
        [
            "--no-store".to_owned(),
            "--session-path".to_owned(),
            "/tmp/session.cbor".to_owned(),
        ]
        .into_iter(),
    )
    .err()
    .expect("must reject");

    assert!(error.to_string().contains("--no-store"));
}

#[test]
fn parses_auth_path_command() {
    let args = parse(&["auth", "path", "--name", "work"]);
    let Command::Auth(AuthCommand::Path { name }) = args.command else {
        panic!("expected auth path command");
    };

    assert_eq!(name, "work");
}

#[test]
fn parses_auth_status_command() {
    let args = parse(&["auth", "status", "--name", "work"]);
    let Command::Auth(AuthCommand::Status { name }) = args.command else {
        panic!("expected auth status command");
    };

    assert_eq!(name, "work");
}

#[test]
fn parses_auth_import_command() {
    let args = parse(&[
        "auth",
        "import",
        "--name",
        "work",
        "--file",
        "/tmp/auth.json",
    ]);
    let Command::Auth(AuthCommand::Import { name, path }) = args.command else {
        panic!("expected auth import command");
    };

    assert_eq!(name, "work");
    assert_eq!(path, Some(PathBuf::from("/tmp/auth.json")));
}

#[test]
fn parses_provider_add_command() {
    let args = parse(&["provider", "add"]);
    let Command::Provider(ProviderCommand::Add) = args.command else {
        panic!("expected provider add command");
    };
}

#[test]
fn parses_provider_list_command() {
    let args = parse(&["provider", "list"]);
    let Command::Provider(ProviderCommand::List) = args.command else {
        panic!("expected provider list command");
    };
}

#[test]
fn parses_provider_status_alias() {
    let args = parse(&["provider", "status"]);
    let Command::Provider(ProviderCommand::List) = args.command else {
        panic!("expected provider status command");
    };
}

#[test]
fn parses_provider_remove_command() {
    let args = parse(&["provider", "remove", "work"]);
    let Command::Provider(ProviderCommand::Remove { name }) = args.command else {
        panic!("expected provider remove command");
    };

    assert_eq!(name, "work");
}

#[test]
fn provider_remove_defaults_to_default_namespace() {
    let args = parse(&["provider", "remove"]);
    let Command::Provider(ProviderCommand::Remove { name }) = args.command else {
        panic!("expected provider remove command");
    };

    assert_eq!(name, DEFAULT_AUTH_NAME);
}

#[test]
fn provider_add_rejects_arguments() {
    let error = Args::parse(
        [
            "provider".to_owned(),
            "add".to_owned(),
            "chatgpt".to_owned(),
        ]
        .into_iter(),
    )
    .err()
    .expect("must reject");

    assert!(error.to_string().contains("does not accept arguments"));
}

#[test]
fn auth_login_is_not_a_command() {
    let error = Args::parse(["auth".to_owned(), "login".to_owned()].into_iter())
        .err()
        .expect("must reject");

    assert!(error.to_string().contains("unknown auth command"));
}

#[test]
fn rejects_unknown_command() {
    let error = Args::parse(["unknown".to_owned()].into_iter())
        .err()
        .expect("must reject");

    assert!(error.to_string().contains("unknown argument"));
}

#[test]
fn provider_tool_call_response_keeps_tool_block_live_until_turn_finish() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    let call = ToolCall {
        id: ToolCallId("call-1".to_owned()),
        name: "shell_command".to_owned(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({"command": "printf hi"}),
    };

    renderer.handle_provider(ResponsesUpdate::ToolCall {
        output_index: 0,
        call: call.clone(),
    });
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.handle_provider(ResponsesUpdate::Finished(ProviderResponse {
        items: vec![ItemKind::ToolCall(call.clone())],
        usage: None,
        provider_response_id: None,
    }));
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.handle_agent(AgentUpdate::ToolCallStarted(call.clone()));
    assert_eq!(renderer.tool_blocks.len(), 1);
    renderer.handle_agent(AgentUpdate::ToolCallFinished(ToolResult::success(
        call.id, "hi",
    )));
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.finish_turn();
    assert!(renderer.tool_blocks.is_empty());
}

#[test]
fn provider_text_response_finalizes_without_outer_loop() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);

    renderer.handle_provider(ResponsesUpdate::TextDelta {
        output_index: 0,
        text: "done".to_owned(),
    });
    assert!(renderer.assistant_block.is_some());

    renderer.handle_provider(ResponsesUpdate::Finished(ProviderResponse {
        items: Vec::new(),
        usage: None,
        provider_response_id: None,
    }));
    assert!(renderer.assistant_block.is_none());
}
