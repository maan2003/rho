//! The SSH Git approval prompt.
//!
//! An agent host that wants to reach out over SSH asks first, and the answer is
//! the user's: approving one is a decision about which machine talks to
//! which. The agent host is blocked on a channel while it waits, so every way
//! out of this state has to answer — allow, deny, the agent host giving up, or
//! the connection dropping. Nothing here may quietly forget a request.
//!
//! It is one of the three modal overlays, with the minibuffer and the
//! menu, and like them it borrows the bottom strip and takes focus while
//! it is up.

use std::collections::VecDeque;

use gpui::prelude::*;
use gpui::{AnyElement, App, FocusHandle, TextStyle, Window, div};
use rho_agent_hosts::HostId;
use rho_agent_hosts::connection::GitApprovalDecision;
use theme::ActiveTheme as _;

/// A request waiting for the user's answer, and the channel the agent host is
/// blocked on. Request ids are each host's own, so the host is part of
/// which request it is.
struct Pending {
    host: HostId,
    request_id: u64,
    prompt: String,
    response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
}

/// The approval prompt: the requests waiting, oldest first. The first is
/// the one shown; the rest wait their turn rather than being refused, since
/// two hosts asking at once is ordinary.
pub(crate) struct GitApproval {
    focus: FocusHandle,
    pending: VecDeque<Pending>,
}

impl GitApproval {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            focus: cx.focus_handle(),
            pending: VecDeque::new(),
        }
    }

    /// The handle the prompt takes focus on while it is up, so `Y` and `n`
    /// reach it rather than the buffer underneath.
    pub(crate) fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Whether a request is waiting for an answer.
    pub(crate) fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Holds a request until the user answers it, behind any already
    /// waiting.
    pub(crate) fn ask(
        &mut self,
        host: HostId,
        request_id: u64,
        prompt: String,
        response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
    ) {
        self.pending.push_back(Pending {
            host,
            request_id,
            prompt,
            response,
        });
    }

    /// Answers the request shown, and says whether there was one. The agent
    /// host is blocked until this happens, which is why every caller that
    /// ends the prompt goes through here.
    pub(crate) fn answer(&mut self, decision: GitApprovalDecision) -> bool {
        let Some(pending) = self.pending.pop_front() else {
            return false;
        };
        let _ = pending.response.send(decision);
        true
    }

    /// The agent host has finished with this request on its own. Says
    /// whether it was the one shown; another host's `Done`, or this host's
    /// for another request, takes nothing else away.
    pub(crate) fn done(&mut self, host: HostId, request_id: u64) -> bool {
        self.settle(|pending| pending.host == host && pending.request_id == request_id)
    }

    /// A host that went away asks nothing any more: its requests are
    /// answered, the other hosts' stay. Says whether the one shown went.
    pub(crate) fn forget_host(&mut self, host: HostId) -> bool {
        self.settle(|pending| pending.host == host)
    }

    fn settle(&mut self, matches: impl Fn(&Pending) -> bool) -> bool {
        let shown = self.pending.front().is_some_and(&matches);
        let (gone, kept) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition::<VecDeque<_>, _>(|pending| matches(pending));
        self.pending = kept;
        for pending in gone {
            let _ = pending.response.send(GitApprovalDecision::Done);
        }
        shown
    }

    /// The prompt as it is drawn in the bottom strip: what is being asked,
    /// and the two ways out. `None` when nothing is waiting.
    pub(crate) fn render(
        &self,
        text_style: &TextStyle,
        window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let pending = self.pending.front()?;
        let colors = cx.theme().colors();
        let mut deny = div().flex().flex_row().px_1().child("n deny");
        if self.focus.is_focused(window) {
            deny = deny.bg(colors.element_selected);
        } else {
            deny = deny.text_color(colors.text_muted);
        }
        Some(
            div()
                .key_context("RhoGitApproval")
                .track_focus(&self.focus)
                .child(
                    crate::minibuffer::bottom_strip(text_style, cx)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .px_2()
                                .child(
                                    div()
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .text_color(colors.text_accent)
                                        .child("Git approval"),
                                )
                                .child("·")
                                .child(pending.prompt.clone())
                                .children((self.pending.len() > 1).then(|| {
                                    div()
                                        .text_color(colors.text_muted)
                                        .child(format!("· {} more", self.pending.len() - 1))
                                })),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_4()
                                .px_2()
                                .child(div().text_color(colors.text_muted).child("Y allow"))
                                .child(deny),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(
        approval: &mut GitApproval,
        host: u32,
        request_id: u64,
    ) -> tokio::sync::oneshot::Receiver<GitApprovalDecision> {
        let (response, answered) = tokio::sync::oneshot::channel();
        approval.ask(
            HostId(host),
            request_id,
            format!("{host}#{request_id}"),
            response,
        );
        answered
    }

    /// Two hosts can both be waiting, with the same request id; each is
    /// answered by what concerns it, and the user's answer goes to the
    /// one shown.
    #[gpui::test]
    fn requests_from_two_hosts_wait_their_turn(cx: &mut gpui::TestAppContext) {
        let mut approval = cx.update(GitApproval::new);
        let mut fern = ask(&mut approval, 0, 1);
        let mut moss = ask(&mut approval, 1, 1);
        let mut moss_later = ask(&mut approval, 1, 2);

        assert!(
            !approval.done(HostId(1), 1),
            "moss's request is not the one shown"
        );
        assert_eq!(moss.try_recv(), Ok(GitApprovalDecision::Done));
        assert!(fern.try_recv().is_err(), "fern still waits");

        assert!(approval.answer(GitApprovalDecision::Allow));
        assert_eq!(fern.try_recv(), Ok(GitApprovalDecision::Allow));
        assert!(approval.waiting(), "moss's second request is next");

        assert!(approval.forget_host(HostId(1)), "the shown request went");
        assert_eq!(moss_later.try_recv(), Ok(GitApprovalDecision::Done));
        assert!(!approval.waiting());
    }
}
