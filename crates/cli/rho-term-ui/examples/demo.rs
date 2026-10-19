//! Try the terminal UI by hand: `cargo run -p rho-term-ui --example demo`.
//!
//! Type a prompt and press Enter: a fake reply streams into a live block above
//! the prompt, then moves to history. Shift-Enter or Alt-Enter inserts a
//! newline, Up/Down recall history, Ctrl-Z/Ctrl-Y undo/redo, Tab toggles the
//! status line, Ctrl-C interrupts a reply, Ctrl-D on an empty prompt exits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use rho_term_ui::{Color, Event, Key, Span, Style, StyledBlock, StyledText, Term, TermHandle};

fn main() -> std::io::Result<()> {
    let (term, handle) = Term::new(Span::new("› ", Style::default().fg(Color::Cyan)))?;
    handle.set_input_placeholder(Span::new(
        "Ask something",
        Style::default().fg(Color::DarkGrey),
    ));
    handle.set_right_prompt(Span::new("demo", Style::default().fg(Color::DarkGrey)));
    let status = handle.new_block(status_line(false));
    handle.push_below(status);
    handle.redraw();

    let cancel = Arc::new(AtomicBool::new(false));
    let mut show_status = true;
    let mut reply: Option<thread::JoinHandle<()>> = None;
    loop {
        match term.get_next_event()? {
            Event::Line(line) if !line.trim().is_empty() => {
                handle.print_output(StyledText::from(vec![
                    Span::new("› ", Style::default().fg(Color::Cyan)),
                    Span::new(line.clone(), Style::default().bold()),
                ]));
                if let Some(previous) = reply.take() {
                    cancel.store(true, Ordering::Relaxed);
                    let _ = previous.join();
                }
                cancel.store(false, Ordering::Relaxed);
                handle.set_block(status, status_line(true));
                let handle = handle.clone();
                let cancel = Arc::clone(&cancel);
                reply = Some(thread::spawn(move || {
                    stream_reply(&handle, &line, &cancel, status)
                }));
            }
            Event::Interrupt => cancel.store(true, Ordering::Relaxed),
            Event::Eof => break,
            Event::Key(Key::Tab) => {
                show_status = !show_status;
                if show_status {
                    handle.push_below(status);
                } else {
                    handle.remove_below(status);
                }
                handle.redraw();
            }
            _ => {}
        }
    }
    cancel.store(true, Ordering::Relaxed);
    if let Some(reply) = reply {
        let _ = reply.join();
    }
    Ok(())
}

/// Streams a canned reply word by word into a live block, then moves the
/// finished block into history.
fn stream_reply(
    handle: &TermHandle,
    prompt: &str,
    cancel: &AtomicBool,
    status: rho_term_ui::BlockId,
) {
    let text = format!(
        "You said “{prompt}”. This reply streams into an above-active block, \
         one word at a time, so it can grow in place next to the prompt. Once \
         it is done it moves into history and scrolls away like ordinary \
         terminal output.\n\nResize the window to watch history reflow."
    );
    let block = handle.new_block("");
    handle.push_above_active(block);
    let mut shown = String::new();
    for word in text.split_inclusive(' ') {
        if cancel.load(Ordering::Relaxed) {
            shown.push_str(" [interrupted]");
            break;
        }
        shown.push_str(word);
        handle.set_block(block, StyledBlock::new(shown.clone()).margin_left(2));
        handle.redraw();
        thread::sleep(Duration::from_millis(40));
    }
    handle.set_block(block, StyledBlock::new(shown).margin_left(2));
    handle.remove_above_active(block);
    handle.push_history(block);
    handle.set_block(status, status_line(false));
    handle.redraw();
}

fn status_line(streaming: bool) -> StyledBlock {
    let state = if streaming { "streaming…" } else { "idle" };
    StyledBlock::new(Span::new(
        format!("{state} · Tab hides this line · Ctrl-D exits"),
        Style::default().fg(Color::DarkGrey),
    ))
    .right_content(Span::new(
        "rho-term-ui",
        Style::default().fg(Color::DarkGrey),
    ))
}
