//! What the client's replica holds, read off a copy.
//!
//! The daemon probe says what the store holds; this says what the client
//! resumes from, which is the other half of "why did that sync carry the
//! whole desk again". It opens the client's database through the same
//! reader the GUI uses, so what it prints is what the GUI would resume
//! with. The GUI must be down: the file takes an exclusive lock.
//!
//!     cargo run -p rho-qa --example desk_replica -- \
//!         /home/maan2003/src/rho-rigs/mv/state/rho local [word]
//!
//! A third argument prints every body whose text carries that word.

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(state_dir) = args.next() else {
        eprintln!("usage: desk_replica <client state dir> [host name]");
        std::process::exit(2);
    };
    if state_dir.contains("/.local/state/") {
        eprintln!("that is the user's own state directory; take a copy and read that");
        std::process::exit(2);
    }
    let host = args.next().unwrap_or_else(|| "local".to_owned());
    let mirror = rho_mirror::desk::DeskMirror::open(std::path::Path::new(&state_dir))
        .expect("open the client database");
    let held = mirror.load(&host);
    println!("host {host}: known={}", held.known);
    if !held.known {
        return;
    }
    println!("store {:?}", held.store);
    println!("namespace {}", held.namespace);
    println!("version {:?}", held.snapshot.version);
    println!(
        "cells {} verdicts {} bodies {} operations {}",
        held.snapshot.cells.len(),
        held.snapshot.verdicts.len(),
        held.bodies.len(),
        held.bodies
            .iter()
            .map(|body| body.operations.len())
            .sum::<usize>(),
    );
    // With a word to look for, the bodies that carry it are printed whole:
    // "the note reads back" is a claim about the text, not about counts.
    let Some(word) = args.next() else { return };
    for body in &held.bodies {
        let Ok(buffer) = body.buffer(1, text::BufferId::new(1).unwrap()) else {
            continue;
        };
        let text = buffer.text();
        if text.contains(&word) {
            println!("body {:?}: {text}", body.id);
        }
    }
}
