//! The SSH Git approval prompt.
//!
//! A daemon that wants to reach out over SSH asks first, and the answer is
//! the user's: approving one is a decision about which machine talks to
//! which. The daemon is blocked on a channel while it waits, so every way
//! out of this state has to answer — allow, deny, the daemon giving up, or
//! the connection dropping. Nothing here may quietly forget a request.
//!
//! It is one of the three modal overlays, with the minibuffer and the
//! menu, and like them it borrows the bottom strip and takes focus while
//! it is up.

use gpui::prelude::*;
use gpui::{AnyElement, App, FocusHandle, TextStyle, Window, div};
use rho_hosts::connection::GitApprovalDecision;
use theme::ActiveTheme as _;

/// A request waiting for the user's answer, and the channel the daemon is
/// blocked on.
struct Pending {
    request_id: u64,
    prompt: String,
    response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
}

/// The approval prompt, up or not.
pub(crate) struct GitApproval {
    focus: FocusHandle,
    pending: Option<Pending>,
}

impl GitApproval {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            focus: cx.focus_handle(),
            pending: None,
        }
    }

    /// The handle the prompt takes focus on while it is up, so `Y` and `n`
    /// reach it rather than the buffer underneath.
    pub(crate) fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Whether a request is waiting for an answer.
    pub(crate) fn waiting(&self) -> bool {
        self.pending.is_some()
    }

    /// Holds a request until the user answers it.
    pub(crate) fn ask(
        &mut self,
        request_id: u64,
        prompt: String,
        response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
    ) {
        self.pending = Some(Pending {
            request_id,
            prompt,
            response,
        });
    }

    /// Answers the waiting request, and says whether there was one. The
    /// daemon is blocked until this happens, which is why every caller
    /// that ends the prompt goes through here.
    pub(crate) fn answer(&mut self, decision: GitApprovalDecision) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let _ = pending.response.send(decision);
        true
    }

    /// The daemon has finished with this request on its own. Answers
    /// whether it was the one being waited on — another request's `Done`
    /// says nothing about this one.
    pub(crate) fn done(&mut self, request_id: u64) -> bool {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            return false;
        }
        self.answer(GitApprovalDecision::Done)
    }

    /// The prompt as it is drawn in the bottom strip: what is being asked,
    /// and the two ways out. `None` when nothing is waiting.
    pub(crate) fn render(
        &self,
        text_style: &TextStyle,
        window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let pending = self.pending.as_ref()?;
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
                                .child(pending.prompt.clone()),
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
