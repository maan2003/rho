//! Which Claude account a host's agents run on: `switch claude account`,
//! from `Commands` on every screen (`usage.Chart`'s `switch claude
//! account`). The user picks a host when more than one is attached, then
//! one of the accounts the host has, each with what is left of its
//! allowance, or names it on the line, and the host's agents run on it
//! from their next turn.

use std::future::Future;
use std::rc::Rc;

use gpui::App;
use rho_agent_hosts::HostId;
use rho_agents_client::protocol::{
    ClaudeAccountList, ClaudeAccounts, QuotaSummary, SetClaudeAccount,
};
use rho_agents_client::remote::AgentsLink;
use rho_frame::{Candidate, Command, Spelling};
use rho_window::style::StyleClass;

pub static SWITCH_CLAUDE_ACCOUNT: Spelling = Spelling {
    name: "switch claude account",
    aliases: &[
        ("claude account", 9),
        ("change account", 8),
        ("account", 7),
        ("log in", 4),
        ("out of quota", 5),
    ],
    argument: Some("account"),
};

const COMMAND: &str = SWITCH_CLAUDE_ACCOUNT.name;

/// The Claude allowances a host reports for each account, in the order
/// the rate limit chart names them.
const ALLOWANCES: [&str; 3] = ["session", "opus", "fable"];

/// What the window knows of a host's allowances: what it last reported.
pub(super) type Reported = Rc<dyn Fn(HostId, &App) -> Vec<QuotaSummary>>;

/// The window's own command: no screen hides it. An account typed after
/// its name is switched to without a prompt, once the host is known.
/// `target` names the host it acts on, as the row shows it.
pub(super) fn command(reported: Reported, target: String) -> Command {
    Command::new(&SWITCH_CLAUDE_ACCOUNT, move |frame, typed, window, cx| {
        let reported = reported.clone();
        frame.choose_host(
            COMMAND,
            Rc::new(move |frame, host, window, cx| {
                let link = AgentsLink::new(frame.hosts().link(host));
                let answer = link.call(ClaudeAccounts);
                let reported = reported.clone();
                let typed = typed.clone();
                frame.when_host_answers(
                    COMMAND,
                    host,
                    answer,
                    move |frame, list, window, cx| {
                        let name = frame.hosts().host_label(host);
                        let prompt = format!("claude account on {name} ({}):", list.current);
                        let rows = Rc::new(account_rows(&list, &reported(host, cx)));
                        let decide = move |frame: &mut rho_frame::Frame,
                                           input: &str,
                                           cx: &mut gpui::Context<rho_frame::Frame>| {
                            match switch(&link, name.clone(), &list, input) {
                                Ok(answer) => {
                                    cx.spawn(async move |this, cx| {
                                        let (said, class) = answer.await;
                                        let _ = this
                                            .update(cx, |frame, cx| frame.echo(&said, class, cx));
                                    })
                                    .detach();
                                }
                                Err(refused) => frame.echo(&refused, StyleClass::SystemInfo, cx),
                            }
                        };
                        if let Some(typed) = &typed {
                            return decide(frame, typed, cx);
                        }
                        frame.open_prompt(
                            prompt,
                            Rc::new(move |_, needle, _| {
                                let needle = needle.trim().to_lowercase();
                                rows.iter()
                                    .filter(|row| row.value.to_lowercase().contains(&needle))
                                    .cloned()
                                    .collect()
                            }),
                            Rc::new(move |frame, input, _, cx| decide(frame, &input, cx)),
                            window,
                            cx,
                        );
                    },
                    window,
                    cx,
                );
            }),
            window,
            cx,
        );
    })
    .target(target)
}

/// One row per account the host has: which one agents run on now, and
/// what is left of each allowance it last reported.
fn account_rows(list: &ClaudeAccountList, summaries: &[QuotaSummary]) -> Vec<Candidate> {
    list.accounts
        .iter()
        .map(|account| {
            let mut description = Vec::new();
            if *account == list.current {
                description.push("current".to_owned());
            }
            let mut reported: Vec<&QuotaSummary> = summaries
                .iter()
                .filter(|summary| summary.auth_namespace.as_deref() == Some(account.as_str()))
                .filter(|summary| ALLOWANCES.contains(&summary.model.as_str()))
                .collect();
            reported
                .sort_by_key(|summary| ALLOWANCES.iter().position(|model| *model == summary.model));
            match reported.is_empty() {
                true => description.push("allowance not reported".to_owned()),
                false => description.extend(reported.iter().map(|summary| {
                    format!("{} {}% left", summary.model, summary.remaining_percent)
                })),
            }
            Candidate {
                value: account.clone(),
                description: description.join(" · "),
            }
        })
        .collect()
}

/// Puts `host`'s agents on the account typed or chosen, and answers with
/// what the echo line says once the host has. Only an account the host
/// listed: a new name would make an account nobody logged in to, and
/// every agent would fail at its next turn.
fn switch(
    link: &AgentsLink,
    host: String,
    list: &ClaudeAccountList,
    input: &str,
) -> Result<impl Future<Output = (String, StyleClass)> + 'static, String> {
    let name = input.trim();
    if !list.accounts.iter().any(|account| account == name) {
        return Err(format!(
            "{COMMAND}: {host} has no account called {name:?} · log in to it on the host with \
             `rho claude-account login {name}`"
        ));
    }
    let answer = link.call(SetClaudeAccount {
        name: name.to_owned(),
    });
    Ok(async move {
        match answer.await {
            Ok(list) => (
                format!(
                    "{host}: agents run on {} from their next turn",
                    list.current
                ),
                StyleClass::SystemInfo,
            ),
            Err(error) => (
                format!("{COMMAND}: {host}: {error:#}"),
                StyleClass::StatusError,
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    use rho_agent_hosts::Link;
    use rho_agents_client::protocol::worksets::{Open, Request};
    use rho_rpc::protocol::{Answer, read_frame, write_frame};

    use super::*;

    fn accounts(current: &str) -> ClaudeAccountList {
        ClaudeAccountList {
            accounts: vec!["home".to_owned(), "work".to_owned(), "spare".to_owned()],
            current: current.to_owned(),
        }
    }

    fn summary(model: &str, account: Option<&str>, remaining_percent: u8) -> QuotaSummary {
        QuotaSummary {
            model: model.to_owned(),
            auth_namespace: account.map(str::to_owned),
            remaining_percent,
            burn_10m: 0,
            burn_2h: 0,
            burn_1d: 0,
            burn_3d: 0,
            reset_at_unix: None,
        }
    }

    /// Each account says whether agents run on it and what is left of the
    /// allowances it reported, in the chart's order; another account's
    /// report, a GPT one of the same name, or an unnamed one is not its.
    #[test]
    fn each_account_shows_its_own_allowance() {
        let summaries = [
            summary("opus", Some("work"), 18),
            summary("session", Some("work"), 64),
            summary("fable", Some("home"), 90),
            summary("gpt", Some("spare"), 5),
            summary("opus", None, 1),
        ];
        let rows: Vec<_> = account_rows(&accounts("work"), &summaries)
            .into_iter()
            .map(|row| (row.value, row.description))
            .collect();
        assert_eq!(
            rows,
            [
                ("home".to_owned(), "fable 90% left".to_owned()),
                (
                    "work".to_owned(),
                    "current · session 64% left · opus 18% left".to_owned()
                ),
                ("spare".to_owned(), "allowance not reported".to_owned()),
            ]
        );
    }

    /// The account chosen is the one the host is told to switch to, and
    /// the echo line says the one the host then runs agents on.
    #[test]
    fn choosing_an_account_switches_the_host_to_it() {
        let link = Link::detached();
        let mut streams = link.connect_in_process();
        let agents = AgentsLink::new(link);
        let answer = switch(&agents, "lab".to_owned(), &accounts("home"), " spare ")
            .unwrap_or_else(|refused| panic!("{refused}"));
        let (asked, said) = futures::executor::block_on(async {
            let host = async {
                let mut stream = streams.recv().await.expect("the window dialed");
                let open: rho_rpc::protocol::Open = read_frame(&mut stream).await.unwrap();
                let Open::Request(request) = open.unpack::<Open>().unwrap() else {
                    panic!("a call");
                };
                let Request::SetClaudeAccount(SetClaudeAccount { name }) = &request else {
                    panic!("asked {request:?}");
                };
                write_frame(&mut stream, &Answer::from(Ok(accounts(name))))
                    .await
                    .unwrap();
                request
            };
            futures::join!(host, answer)
        });
        assert_eq!(
            asked,
            Request::SetClaudeAccount(SetClaudeAccount {
                name: "spare".to_owned()
            })
        );
        assert_eq!(
            said,
            (
                "lab: agents run on spare from their next turn".to_owned(),
                StyleClass::SystemInfo
            )
        );
    }

    /// A name the host did not list is refused here, before the host is
    /// asked: switching would make an account nobody logged in to.
    #[test]
    fn a_name_the_host_did_not_list_asks_nothing() {
        let link = Link::detached();
        let mut streams = link.connect_in_process();
        let agents = AgentsLink::new(link);
        let refused = switch(&agents, "lab".to_owned(), &accounts("home"), "wrok")
            .err()
            .expect("refused");
        assert_eq!(
            refused,
            "switch claude account: lab has no account called \"wrok\" · log in to it on the \
             host with `rho claude-account login wrok`"
        );
        assert!(streams.try_recv().is_err(), "the host was not dialed");
    }

    /// A host that is not there is named with why on the echo line.
    #[test]
    fn a_lost_host_says_why() {
        let answer = switch(
            &AgentsLink::detached(),
            "lab".to_owned(),
            &accounts("home"),
            "work",
        )
        .unwrap_or_else(|refused| panic!("{refused}"));
        assert_eq!(
            futures::executor::block_on(answer),
            (
                "switch claude account: lab: not connected to an agent host".to_owned(),
                StyleClass::StatusError
            )
        );
    }
}

/// The row as the window runs it: through the hosts the frame holds, to a
/// host in this process.
#[cfg(test)]
mod window_tests {
    use std::cell::RefCell;

    use gpui::{TestAppContext, WindowHandle};
    use rho_agent_hosts::{AttachTarget, HostSpec};
    use rho_agents_client::protocol::worksets::{Open, Request};
    use rho_frame::Frame;
    use rho_rpc::protocol::{Answer, Protocol, read_frame, write_frame};

    use super::*;

    fn accounts(current: &str) -> ClaudeAccountList {
        ClaudeAccountList {
            accounts: vec!["home".to_owned(), "work".to_owned()],
            current: current.to_owned(),
        }
    }

    fn window(cx: &mut TestAppContext) -> WindowHandle<Frame> {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            gpui_tokio::init(cx);
        });
        let window = cx.add_window(|window, cx| {
            Frame::new(
                rho_db::RhoDb::in_memory(),
                Vec::new(),
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
    }

    type Asked = Rc<RefCell<Vec<Request>>>;

    /// Attaches a host called `name` and puts it in this process. It
    /// answers the account calls, says it runs on `home`, and closes every
    /// other stream.
    fn attach(
        window: &WindowHandle<Frame>,
        name: &str,
        cx: &mut TestAppContext,
    ) -> (HostId, Asked) {
        let (host, streams) = window
            .update(cx, |frame, _, cx| {
                let host = frame.attach_host(
                    HostSpec {
                        name: name.to_owned(),
                        target: AttachTarget::Local {
                            runtime_dir: format!("/nonexistent/{name}").into(),
                        },
                    },
                    cx,
                );
                let link = frame.hosts().connection(host).unwrap().link();
                (host, link.connect_in_process())
            })
            .unwrap();
        let asked = Asked::default();
        let record = asked.clone();
        cx.foreground_executor()
            .spawn(async move {
                let mut streams = streams;
                while let Some(mut stream) = streams.recv().await {
                    let open: rho_rpc::protocol::Open = read_frame(&mut stream).await.unwrap();
                    if open.protocol != Protocol::Worksets {
                        continue;
                    }
                    let Ok(Open::Request(request)) = open.unpack::<Open>() else {
                        continue;
                    };
                    record.borrow_mut().push(request.clone());
                    let list = match request {
                        Request::ClaudeAccounts(ClaudeAccounts) => accounts("home"),
                        Request::SetClaudeAccount(SetClaudeAccount { name }) => accounts(&name),
                        _ => continue,
                    };
                    write_frame(&mut stream, &Answer::from(Ok(list)))
                        .await
                        .unwrap();
                }
            })
            .detach();
        (host, asked)
    }

    /// Runs the command, with `reported` as what the hosts said is left
    /// and `typed` after its name on the line.
    fn run_command(
        window: &WindowHandle<Frame>,
        reported: Reported,
        typed: Option<&str>,
        cx: &mut TestAppContext,
    ) {
        window
            .update(cx, |frame, window, cx| {
                command(reported, String::new()).run(typed.map(str::to_owned), frame, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
    }

    fn nothing_reported() -> Reported {
        Rc::new(|_, _| Vec::new())
    }

    fn prompt(window: &WindowHandle<Frame>, cx: &mut TestAppContext) -> Option<String> {
        window
            .update(cx, |frame, _, _| {
                frame
                    .minibuffer_mut()
                    .map(|prompt| prompt.prompt().to_owned())
            })
            .unwrap()
    }

    fn answer(window: &WindowHandle<Frame>, text: &str, cx: &mut TestAppContext) {
        window
            .update(cx, |frame, window, cx| {
                frame.minibuffer_mut().expect("a prompt is open").set_input(
                    text.to_owned(),
                    window,
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action((*window).into(), rho_frame::keys::MinibufferConfirm);
        cx.run_until_parked();
    }

    /// With no host attached the echo line says so and nothing is asked.
    #[gpui::test]
    fn no_host_asks_nothing(cx: &mut TestAppContext) {
        let window = window(cx);
        run_command(&window, nothing_reported(), None, cx);
        assert_eq!(prompt(&window, cx), None);
    }

    /// With one host attached the command asks it at once, offers its accounts
    /// with what is left of each, and switches it to the one chosen.
    #[gpui::test]
    fn one_host_switches_without_a_host_prompt(cx: &mut TestAppContext) {
        let window = window(cx);
        let (lab, asked) = attach(&window, "lab", cx);
        let reported: Reported = Rc::new(move |host, _| {
            assert_eq!(host, lab, "what the chosen host reported");
            vec![QuotaSummary {
                model: "session".to_owned(),
                auth_namespace: Some("home".to_owned()),
                remaining_percent: 64,
                burn_10m: 0,
                burn_2h: 0,
                burn_1d: 0,
                burn_3d: 0,
                reset_at_unix: None,
            }]
        });
        run_command(&window, reported, None, cx);
        assert_eq!(*asked.borrow(), [Request::ClaudeAccounts(ClaudeAccounts)]);
        assert_eq!(
            prompt(&window, cx).as_deref(),
            Some("claude account on lab (home):")
        );
        let first = window
            .update(cx, |frame, _, _| {
                frame.minibuffer_mut().unwrap().selected_candidate()
            })
            .unwrap()
            .map(|(row, _)| (row.value, row.description));
        assert_eq!(
            first,
            Some(("home".to_owned(), "current · session 64% left".to_owned()))
        );
        answer(&window, "work", cx);
        assert_eq!(
            *asked.borrow(),
            [
                Request::ClaudeAccounts(ClaudeAccounts),
                Request::SetClaudeAccount(SetClaudeAccount {
                    name: "work".to_owned()
                }),
            ]
        );
        assert_eq!(prompt(&window, cx), None);
    }

    /// An account named on the line is switched to without a prompt; one
    /// the host does not have is refused, and nothing is switched.
    #[gpui::test]
    fn an_account_typed_on_the_line_switches_at_once(cx: &mut TestAppContext) {
        let window = window(cx);
        let (_, asked) = attach(&window, "lab", cx);
        run_command(&window, nothing_reported(), Some("work"), cx);
        assert_eq!(prompt(&window, cx), None);
        assert_eq!(
            *asked.borrow(),
            [
                Request::ClaudeAccounts(ClaudeAccounts),
                Request::SetClaudeAccount(SetClaudeAccount {
                    name: "work".to_owned()
                }),
            ]
        );
        run_command(&window, nothing_reported(), Some("nobody"), cx);
        assert_eq!(prompt(&window, cx), None);
        assert_eq!(
            asked.borrow().len(),
            3,
            "asked for the list, switched nothing"
        );
        assert!(
            matches!(asked.borrow().last(), Some(Request::ClaudeAccounts(_))),
            "the last request only listed the accounts"
        );
    }

    /// With two hosts attached the command asks `host:` first, and only the
    /// chosen host is asked and switched.
    #[gpui::test]
    fn two_hosts_ask_which_first(cx: &mut TestAppContext) {
        let window = window(cx);
        let (_, lab) = attach(&window, "lab", cx);
        let (_, sea) = attach(&window, "sea", cx);
        run_command(&window, nothing_reported(), None, cx);
        assert_eq!(prompt(&window, cx).as_deref(), Some("host:"));
        assert!(lab.borrow().is_empty() && sea.borrow().is_empty());
        answer(&window, "sea", cx);
        assert_eq!(
            prompt(&window, cx).as_deref(),
            Some("claude account on sea (home):")
        );
        answer(&window, "work", cx);
        assert!(lab.borrow().is_empty(), "the other host is not asked");
        assert_eq!(
            *sea.borrow(),
            [
                Request::ClaudeAccounts(ClaudeAccounts),
                Request::SetClaudeAccount(SetClaudeAccount {
                    name: "work".to_owned()
                }),
            ]
        );
    }
}
