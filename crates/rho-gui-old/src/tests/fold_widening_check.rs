//! Whether the fold map's widening check can say anything.
//!
//! `FoldMap::sync` now records every widened edit that stopped describing a
//! range, at the point the edit is built rather than at the seek that later
//! trips over it. That check lives in the vendored editor, which is not a
//! workspace member and cannot run its own tests here, so it would
//! otherwise be an instrument nobody had ever seen produce a reading —
//! which is the one kind of check worth distrusting.
//!
//! So the predicate is exported under `wrap-test-support` and both of its
//! answers are taken here, on all three cases: a range that starts after it
//! ends, a range that reaches past the tree, and a healthy one.

/// The check fires on each fault it is for and stays silent on a range that
/// is fine.
#[test]
fn the_widening_check_says_both_things() {
    use editor::display_map::widening_violation;

    // An unsigned subtraction that took more than the side had. This is the
    // underflow at a fold beginning at offset zero: 5 minus 219, which in
    // release is about eighteen quintillion and here is simply a start past
    // its own end.
    // Built rather than written as `900..100`, because an inverted range
    // literal is a mistake everywhere except here, where it is the input
    // under test.
    let inverted = widening_violation(
        "old",
        std::ops::Range {
            start: 900,
            end: 100,
        },
        4_000,
        "fold widening",
    );
    assert!(
        inverted.is_some_and(|said| said.contains("starts after it ends")),
        "a range whose start is past its end has to be reported"
    );

    // A range that runs off the end of the document it is a range of.
    let past_the_end = widening_violation("new", 100..5_000, 4_000, "fold widening");
    assert!(
        past_the_end.is_some_and(|said| said.contains("past the 4000 bytes")),
        "a range past the tree's extent has to be reported"
    );

    // And the answer that must not be a violation, or every sync reports one
    // and the record is worthless. The empty range at the very end is
    // included because it is the ordinary shape of an edit at the end of a
    // document and would be the first false positive.
    assert!(widening_violation("old", 100..900, 4_000, "fold widening").is_none());
    assert!(widening_violation("old", 4_000..4_000, 4_000, "fold widening").is_none());
    assert!(widening_violation("new", 0..0, 0, "fold widening").is_none());
}
