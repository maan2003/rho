# User journeys

A journey is one goal, from a named starting screen, to a named end state,
with its cost in keystrokes. The rules the journeys are written against:

- **TikTok is the golden rule for up and down.** Up (`f21`, `ctrl-k`) moves
  back through history. Down (`f20`, `ctrl-j`) moves forward through history
  when there is anything forward, and only at the newest entry does down
  deal: it opens the next thing that asks for attention and appends it.
  There is no other next or previous between agents.
- **Every screen is a buffer with the point.** Leaving and coming back finds
  the point, the scroll and the folds where they were.
- **The cost is counted.** `today` is measured on the rig by QA (keystrokes
  from the start screen, end frame kept); `target` is the user's. A change
  that touches a journey names it in its landing note.

Format:

```
Jn  goal
    from:    the start screen and where the point is
    end:     the screen and where the point is
    today:   keys, N keystrokes            (measured; session named)
    target:  keys, M keystrokes
    rule:    what must hold on the way
```

## J1  Deal with what asks for attention

    from:    anywhere, at the newest history entry
    end:     the next surface that asks for attention, point on the thing to answer
    today:   f20, 1 keystroke per item (rig session 99, on main 5c10ee56 plus the
             history change). Three cards dealt with three presses: notes 174, 38,
             138, each `surface_shown ... method deal` in the journal, each appended
             so the list read len 4 with Home under them. Down at the newest is the
             only way in: nothing else was pressed. Up from the dealt surface is
             where the reader was, byte-identical (session 97). Meets target.
             The deal that opens agent qws41uh6dpog's transcript killed the GUI on
             two runs of three — an editor layout panic, not this journey's and not
             this change's; see the landing note.
    target:  f20, 1 keystroke per item; the answer itself is the only other input
    rule:    down at the newest entry deals; the dealt surface is appended, so up
             from it is where the reader was; when nothing asks, down says so in the
             echo area and moves nothing

## J2  Go back and forward through history

    from:    any surface reached through J1 or J3..J9
    end:     the same surface, byte-identical, point where it was
    today:   f21 x n then f20 x n, 2n keystrokes, every stop byte-identical
             (rig session 99). Open A, B, C by dealing; `f21`, `f21`, `f20`, `f20`
             gives B, A, B, C and **all four frames are byte-identical** to the
             originals, against a no-input control that is byte-identical to itself.
             The journal reads back, back, forward, forward at positions 2, 1, 2, 3
             of 4. Across a context change (session 100): agent transcript
             8gpri7fqusxg, Slack list, Slack conversation G1, then `f21`, `f21` —
             the list then the transcript, both byte-identical, back walking out of
             Slack and into the agent. A new open with the cursor mid-list
             **appends and does not truncate**, which is what the old workspace
             history at 81318e26 did. Meets target.
             The cost before this change was one list per context, so a deal into a
             new agent left back with nothing behind it; and a step to a dealt note
             moved the title bar and not the point, so the reader was told they had
             moved and shown the rows they were already reading.
    target:  f21 x n then f20 x n, 2n keystrokes, every pair byte-identical
    rule:    one list across contexts; back walks into the context the reader came
             from; a new open with the cursor mid-list behaves as the old workspace
             history did (QA names which); forget by key never breaks the walk

## J3  Start an agent from an idea

    from:    (a) an agent transcript, point anywhere
             (b) Home, point on a repo or host row
             (c) a Slack conversation
    end:     the new agent's transcript, point in its prompt
    today:   pending measurement per branch
    target:  space n, the prompt, enter: 2 keystrokes plus the prompt; the host and
             repo default from where the reader is, and the minibuffer asks only for
             what cannot be defaulted
    rule:    never leaves the buffer to a modal; up from the new transcript is where
             the idea came from

## J4  Answer a verdict without leaving what I was reading

    from:    a transcript with a verdict asked, point anywhere
    end:     the same transcript, point where it was, verdict answered
    today:   pending measurement
    target:  the verdict transient, one key: 1 keystroke
    rule:    the buffer above the transient does not move; the answer is echoed,
             never confirmed by a modal

## J5  Read and answer a Slack conversation

    from:    the Slack list, point on a conversation with unread
    end:     the list again, that conversation marked read and sorted accordingly
    today:   pending measurement
    target:  enter, the reply, enter, f21: 3 keystrokes plus the reply
    rule:    mark read sticks; the list row moves once per event, not the list

## J6  Check usage and come back

    from:    any surface
    end:     the same surface, byte-identical
    today:   space s u r then f21, 5 keystrokes (rig session 101, from Home).
             The frame after `f21` is **byte-identical** to the frame before
             `space`, against a no-input control that is byte-identical to itself.
             Three charts deep — `space s u r`, `space s u c`, `space s u a`, 12
             keystrokes — `f21` is still one keystroke and still byte-identical, and
             the journal reads one `history_stepped back position 0 len 2` for each
             return: the usage screen is one surface however many charts were drawn
             on it. Meets target.
    target:  space s u r (a chart), f21: 5 keystrokes
    rule:    the usage screen is one surface; another chart redraws it, so back
             is one step regardless of how many charts were looked at

## J7  Read the top of a long transcript and return to the tail

    from:    a transcript at its tail
    end:     the tail, point where it was
    today:   pending measurement
    target:  g g, read, G: 3 keystrokes
    rule:    the head composes first and the gap closes behind the reader with no
             frame over 8 ms; folds at the head are the same as at the tail

## J8  Give an agent a file or an image

    from:    an agent transcript or its draft
    end:     the draft with the attachment chip, point after it
    today:   pending measurement
    target:  paste, or space f then the path in the minibuffer: 1 to 2 keystrokes
             plus the path
    rule:    the chip is a measured block; the draft above it does not move

## J9  Find an agent by name or repo

    from:    anywhere
    end:     that agent's transcript, point where it last was
    today:   the keystroke count is already the target; what was wrong was the
             frame. 445 candidates (145 desk + 300 slack, at 128 agents and the
             fake Slack default world): the open 1.86 ms, a keystroke 3.53 ms,
             and a keystroke down the path this replaced 5.48 ms. Measured by
             find_cost in a debug harness, not a rig session, and part of it is
             gpui's test-support recording per primitive, so the absolute
             figures fall when that is gated; the comparison between the two
             paths is taken in the same run and does not. Two runs on a quiet
             machine agree to within 5%; a third, taken while the machine was
             building under load, read half again as high across all three
             figures, which is the reason for saying so here rather than
             quoting one run.
    target:  the find minibuffer, the first letters, enter: 2 keystrokes plus the
             letters
    rule:    a keystroke ranks what is already in hand and rebuilds nothing; the
             candidate set is taken once and updated per event, never per
             character; enter is the only confirmation
    cost:    ranking is O(candidates) per keystroke — a fuzzy match scores every
             candidate, and no implementation makes that O(log n). The frame bound
             is met by keeping the set small, which is the matters rule, not by the
             scorer.
