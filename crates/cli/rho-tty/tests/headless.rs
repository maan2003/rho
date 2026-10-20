//! The headless terminal draws what a real one would, and takes keys.

use gpui::{
    App, AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, KeyBinding,
    ParentElement as _, Render, Styled as _, Window, WindowOptions, actions, div, px,
};

actions!(test, [Count]);

struct Counter {
    count: usize,
    focus: FocusHandle,
}

impl Render for Counter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Counter")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Count, _, cx| {
                this.count += 1;
                cx.notify();
            }))
            .size_full()
            .text_size(px(rho_tty::FONT_SIZE))
            .line_height(px(rho_tty::CELL_H))
            .flex()
            .flex_col()
            .child("first line")
            .child(div().pl(px(16.0)).child(format!("count {}", self.count)))
    }
}

#[test]
fn a_frame_lands_in_memory_and_keys_reach_the_view() {
    let (app, term) = rho_tty::headless(20, 4);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let record = seen.clone();
    app.run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("ctrl-a", Count, Some("Counter"))]);
        cx.open_window(WindowOptions::default(), |window, cx| {
            let counter = cx.new(|cx| Counter {
                count: 0,
                focus: cx.focus_handle(),
            });
            window.focus(&counter.read(cx).focus.clone(), cx);
            counter
        })
        .unwrap();
        cx.spawn(async move |cx| {
            term.settled().await;
            record.borrow_mut().push(term.text());
            term.press("ctrl-a");
            term.press("ctrl-a");
            term.settled().await;
            record.borrow_mut().push(term.text());
            term.resize(12, 2);
            term.settled().await;
            record.borrow_mut().push(term.text());
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    let seen = seen.take();
    assert_eq!(seen[0], "first line\n  count 0\n\n");
    assert_eq!(seen[1], "first line\n  count 2\n\n");
    assert_eq!(seen[2], "first line\n  count 2");
}
