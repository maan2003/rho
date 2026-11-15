//! Slack discovery through Rho's minibuffer.
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{Context, Window};
use rho_slack::session::Source;
use rho_slack::types::Conversation;
use theme::ActiveTheme;

use crate::minibuffer::Candidate;
use crate::workspace::{SurfaceView, Workspace};

impl Workspace {
    pub(crate) fn render_slack_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let list = self.slack_list_view(window, cx)?;
        let open_list = list.clone();
        Some(
            gpui::div()
                .id("slack-sidebar")
                .w(gpui::relative(0.25))
                .max_w(gpui::px(300.))
                .min_w(gpui::px(180.))
                .h_full()
                .flex_none()
                .overflow_hidden()
                .border_r_1()
                .border_color(cx.theme().colors().border_variant.opacity(0.6))
                .on_action(
                    cx.listener(move |this, _: &crate::SlackOpenRow, window, cx| {
                        let source = open_list.update(cx, |list, cx| list.cursor_source(cx));
                        if let Some(source) = source {
                            this.open_slack_source(source, window, cx);
                        }
                    }),
                )
                .on_action(cx.listener(|this, _: &crate::SurfaceClose, window, cx| {
                    this.focus_active_surface(window, cx);
                }))
                .on_action(cx.listener(|this, _: &crate::SlackSearch, window, cx| {
                    this.open_slack(window, cx);
                    this.prompt_slack_search(window, cx);
                }))
                .child(list)
                .into_any_element(),
        )
    }

    pub(crate) fn prompt_slack_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.slack_session(window, cx).is_none() {
            return;
        }
        self.open_prompt(
            "Jump to conversation:",
            Rc::new(|this, query, cx| {
                let Some(session) = this.slack.session() else {
                    return Vec::new();
                };
                let model = session.read(cx).model();
                let query = query.to_lowercase();
                model
                    .conversation_window(0, model.conversation_count())
                    .into_iter()
                    .filter(|row| row.label.to_lowercase().contains(&query))
                    .map(|row| Candidate {
                        value: row.label,
                        description: [
                            session.read(cx).favorite(&row.id).then_some("starred"),
                            row.unread.then_some("unread"),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                    })
                    .collect()
            }),
            Rc::new(|this, input, window, cx| {
                let Some(session) = this.slack_session(window, cx) else {
                    return;
                };
                let model = session.read(cx).model();
                let rows = model.conversation_window(0, model.conversation_count());
                let row = rows
                    .iter()
                    .find(|row| row.label.eq_ignore_ascii_case(input.trim()))
                    .or_else(|| {
                        rows.iter().find(|row| {
                            row.label
                                .to_lowercase()
                                .contains(&input.trim().to_lowercase())
                        })
                    });
                if let Some(row) = row {
                    this.open_slack_source(Source::Conversation(row.id.clone()), window, cx);
                }
            }),
            window,
            cx,
        );
        if let Some(prompt) = &mut self.minibuffer {
            prompt.set_complete_whole_input();
        }
    }

    pub(crate) fn prompt_slack_people(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.slack_session(window, cx).is_none() {
            return;
        }
        self.open_prompt(
            "New message — people (comma separated):",
            Rc::new(|this, query, cx| {
                let Some(session) = this.slack.session() else {
                    return Vec::new();
                };
                let (prefix, needle) = query
                    .rsplit_once(',')
                    .map_or(("", query), |(prefix, needle)| (prefix, needle));
                let needle = needle.trim().trim_start_matches('@').to_lowercase();
                session
                    .read(cx)
                    .model()
                    .people()
                    .into_iter()
                    .filter(|user| {
                        user.name.to_lowercase().contains(&needle)
                            || user.handle.to_lowercase().contains(&needle)
                    })
                    .map(|user| Candidate {
                        value: format!(
                            "{}@{}",
                            if prefix.is_empty() {
                                String::new()
                            } else {
                                format!("{prefix}, ")
                            },
                            user.handle
                        ),
                        description: user.name,
                    })
                    .collect()
            }),
            Rc::new(|this, input, window, cx| {
                let Some(session) = this.slack_session(window, cx) else {
                    return;
                };
                let people = session.read(cx).model().people();
                let mut recipients = Vec::new();
                for name in input
                    .split(',')
                    .map(|name| name.trim().trim_start_matches('@'))
                {
                    let matches: Vec<_> = people
                        .iter()
                        .filter(|user| {
                            user.handle.eq_ignore_ascii_case(name)
                                || user.name.eq_ignore_ascii_case(name)
                        })
                        .collect();
                    if matches.len() != 1 {
                        this.echo(
                            "Choose each person's exact @handle; separate people with commas",
                            rho_window::style::StyleClass::SystemInfo,
                            cx,
                        );
                        return;
                    }
                    if !recipients.contains(&matches[0].id) {
                        recipients.push(matches[0].id.clone());
                    }
                }
                if recipients.is_empty() || recipients.len() > 8 {
                    this.echo(
                        "Choose one to eight people",
                        rho_window::style::StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                session.update(cx, |session, cx| session.open_direct(recipients, cx));
            }),
            window,
            cx,
        );
        if let Some(prompt) = &mut self.minibuffer {
            prompt.set_complete_whole_input();
        }
    }

    pub(crate) fn slack_browse_channels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.slack_session(window, cx) {
            self.echo(
                "Loading channel directory…",
                rho_window::style::StyleClass::SystemInfo,
                cx,
            );
            session.update(cx, |session, cx| session.browse_channels(cx));
        }
    }

    pub(crate) fn prompt_slack_directory(
        &mut self,
        channels: Vec<Conversation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let channels = Rc::new(channels);
        let choices = channels.clone();
        self.open_prompt(
            "Browse channels — choose to join/open:",
            Rc::new(move |_, query, _| {
                let query = query.trim_start_matches('#').to_lowercase();
                choices
                    .iter()
                    .filter(|channel| channel.name.to_lowercase().contains(&query))
                    .map(|channel| Candidate {
                        value: format!("#{}", channel.name),
                        description: "join / open".into(),
                    })
                    .collect()
            }),
            Rc::new(move |this, input, window, cx| {
                let Some(channel) = channels.iter().find(|channel| {
                    channel
                        .name
                        .eq_ignore_ascii_case(input.trim().trim_start_matches('#'))
                }) else {
                    return;
                };
                if let Some(session) = this.slack_session(window, cx) {
                    session.update(cx, |session, cx| {
                        session.join_channel(channel.id.clone(), cx)
                    });
                }
            }),
            window,
            cx,
        );
        if let Some(prompt) = &mut self.minibuffer {
            prompt.set_complete_whole_input();
        }
    }

    pub(crate) fn slack_toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        if let SurfaceView::SlackConversation(view) = &self.active_surface().view {
            view.clone().update(cx, |view, cx| {
                view.set_also_send_to_channel(!view.also_send_to_channel(), cx);
            });
        }
    }

    pub(crate) fn slack_toggle_favorite(&mut self, cx: &mut Context<Self>) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let channel = view.read(cx).source().channel().clone();
        if let Some(session) = self.slack.session() {
            session.update(cx, |session, cx| session.toggle_favorite(&channel, cx));
            let label = if session.read(cx).favorite(&channel) {
                "Conversation starred"
            } else {
                "Conversation unstarred"
            };
            self.echo(label, rho_window::style::StyleClass::SystemInfo, cx);
        }
    }

    pub(crate) fn slack_toggle_follow(&mut self, cx: &mut Context<Self>) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let Source::Thread(key) = view.read(cx).source().clone() else {
            return;
        };
        if let Some(session) = self.slack.session() {
            session.update(cx, |session, cx| {
                if session.model().follows(&key) {
                    session.ignore_thread(&key, cx);
                } else {
                    session.follow_thread(&key, cx);
                }
            });
        }
    }

    pub(crate) fn prompt_slack_detach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let view = view.clone();
        let files = view.read(cx).attachments();
        let choices = files
            .iter()
            .enumerate()
            .map(|(index, file)| Candidate {
                value: (index + 1).to_string(),
                description: file.name.clone(),
            })
            .collect::<Vec<_>>();
        self.open_prompt(
            "Remove attachment:",
            Rc::new(move |_, query, _| {
                choices
                    .iter()
                    .filter(|choice| choice.value.starts_with(query))
                    .cloned()
                    .collect()
            }),
            Rc::new(move |_, input, _, cx| {
                if let Some(index) = input
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                {
                    view.update(cx, |view, cx| {
                        view.remove_attachment(index, cx);
                    });
                }
            }),
            window,
            cx,
        );
    }
}
