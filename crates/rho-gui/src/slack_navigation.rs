//! Slack's familiar navigation around the conversation editor.
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{Context, Window, div, px, uniform_list};
use rho_slack::session::Source;
use rho_slack::types::{Conversation, ConversationKind};
use theme::ActiveTheme;

use crate::minibuffer::Candidate;
use crate::workspace::{SurfaceView, Workspace};

impl Workspace {
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
                        description: if row.unread {
                            "unread".into()
                        } else {
                            String::new()
                        },
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

    pub(crate) fn render_slack_sidebar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(session) = self.slack.session() else {
            return div().into_any_element();
        };
        let session = session.read(cx);
        let model = session.model();
        let mut rows = model.conversation_window(0, model.conversation_count());
        rows.sort_by_key(|row| {
            let section = if session.favorite(&row.id) {
                0
            } else if model
                .conversation(&row.id)
                .is_some_and(|c| c.kind == ConversationKind::Channel)
            {
                1
            } else {
                2
            };
            (section, row.label.to_lowercase())
        });
        let mut entries = Vec::new();
        let mut previous = None;
        for row in rows {
            let section = if session.favorite(&row.id) {
                0
            } else if model
                .conversation(&row.id)
                .is_some_and(|c| c.kind == ConversationKind::Channel)
            {
                1
            } else {
                2
            };
            if previous != Some(section) {
                entries.push((
                    None,
                    ["Starred", "Channels", "Direct messages"][section].to_owned(),
                    false,
                ));
                previous = Some(section);
            }
            let badge = if row.mention_count > 0 {
                format!("  @{}", row.mention_count)
            } else if row.unread_count > 0 {
                format!("  {}", row.unread_count)
            } else if row.unread {
                "  ●".into()
            } else {
                String::new()
            };
            entries.push((Some(row.id), format!("{}{badge}", row.label), row.unread));
        }
        let entries = Rc::new(entries);
        let selected = match &self.active_surface().view {
            SurfaceView::SlackConversation(view) => Some(view.read(cx).source().channel().clone()),
            _ => None,
        };
        let colors = cx.theme().colors();
        div()
            .id("slack-sidebar")
            .w(px(240.))
            .min_w(px(180.))
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.panel_background)
            .text_color(colors.text)
            .border_r_1()
            .border_color(colors.border)
            .p_2()
            .gap_1()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(model.workspace().0.clone()),
            )
            .child(
                div()
                    .id("slack-jump")
                    .p_1()
                    .cursor_pointer()
                    .child("Jump to…  Ctrl-P")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.prompt_slack_switch(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("slack-find")
                    .p_1()
                    .cursor_pointer()
                    .child("Search messages")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.prompt_slack_find(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("slack-new-dm")
                    .p_1()
                    .cursor_pointer()
                    .child("New message  Ctrl-N")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.prompt_slack_people(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("slack-browse")
                    .p_1()
                    .cursor_pointer()
                    .child("Browse channels")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.slack_browse_channels(window, cx)),
                    ),
            )
            .child(
                uniform_list(
                    "slack-sidebar-rooms",
                    entries.len(),
                    cx.processor(
                        move |this,
                              range: std::ops::Range<usize>,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            range
                                .map(|index| {
                                    let (channel, label, unread) = &entries[index];
                                    let mut row = div()
                                        .id(("slack-room", index))
                                        .h(px(28.))
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .overflow_hidden()
                                        .child(label.clone());
                                    if *unread {
                                        row = row.font_weight(gpui::FontWeight::BOLD);
                                    }
                                    if let Some(channel) = channel {
                                        let channel = channel.clone();
                                        if selected.as_ref() == Some(&channel) {
                                            row = row.bg(cx.theme().colors().element_selected);
                                        }
                                        row = row
                                            .cursor_pointer()
                                            .hover(|row| row.bg(cx.theme().colors().element_hover))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.open_slack_source(
                                                    Source::Conversation(channel.clone()),
                                                    window,
                                                    cx,
                                                )
                                            }));
                                    } else {
                                        row = row
                                            .text_color(cx.theme().colors().text_muted)
                                            .font_weight(gpui::FontWeight::BOLD);
                                    }
                                    let _ = this;
                                    row
                                })
                                .collect()
                        },
                    ),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    pub(crate) fn render_slack_header(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let source = match &self.active_surface().view {
            SurfaceView::SlackConversation(view) => Some(view.read(cx).source().clone()),
            _ => None,
        };
        let label = source
            .as_ref()
            .and_then(|source| {
                self.slack
                    .session()
                    .map(|session| session.read(cx).label(source))
            })
            .unwrap_or_else(|| "Slack".into());
        let star = source.as_ref().is_some_and(|source| {
            self.slack
                .session()
                .is_some_and(|session| session.read(cx).favorite(source.channel()))
        });
        let thread = source.as_ref().and_then(|source| match source {
            Source::Thread(key) => Some(key.clone()),
            _ => None,
        });
        let follows = thread.as_ref().is_some_and(|key| {
            self.slack
                .session()
                .is_some_and(|session| session.read(cx).model().follows(key))
        });
        let colors = cx.theme().colors();
        div()
            .id("slack-header")
            .flex()
            .items_center()
            .gap_3()
            .p_2()
            .text_color(colors.text)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("slack-back")
                    .cursor_pointer()
                    .child("←")
                    .on_click(cx.listener(|_this, _, window, cx| {
                        window.dispatch_action(Box::new(crate::SurfaceBack), cx)
                    })),
            )
            .child(
                div()
                    .id("slack-forward")
                    .cursor_pointer()
                    .child("→")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.active_pane().at_newest() {
                            this.cmd_surface_forward_or_deal(window, cx);
                        }
                    })),
            )
            .child(div().font_weight(gpui::FontWeight::BOLD).child(label))
            .when_some(thread, |header, key| {
                let channel = key.channel.clone();
                header
                    .child(
                        div()
                            .id("slack-thread-channel")
                            .cursor_pointer()
                            .child("Back to channel")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_slack_source(
                                    Source::Conversation(channel.clone()),
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        div()
                            .id("slack-thread-follow")
                            .cursor_pointer()
                            .child(if follows {
                                "Unfollow thread"
                            } else {
                                "Follow thread"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(session) = this.slack.session() {
                                    session.update(cx, |session, cx| {
                                        if session.model().follows(&key) {
                                            session.ignore_thread(&key, cx);
                                        } else {
                                            session.follow_thread(&key, cx);
                                        }
                                    });
                                }
                            })),
                    )
            })
            .when_some(source, |header, source| {
                header
                    .child(
                        div()
                            .id("slack-star")
                            .cursor_pointer()
                            .child(if star { "★" } else { "☆" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(session) = this.slack.session() {
                                    session.update(cx, |session, cx| {
                                        session.toggle_favorite(source.channel(), cx)
                                    });
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("slack-write")
                            .cursor_pointer()
                            .child("Write a message")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.slack_compose(window, cx)),
                            ),
                    )
            })
            .into_any_element()
    }
}
