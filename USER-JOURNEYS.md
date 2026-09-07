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
    today:   pending measurement
    target:  f20, 1 keystroke per item; the answer itself is the only other input
    rule:    down at the newest entry deals; the dealt surface is appended, so up
             from it is where the reader was; when nothing asks, down says so in the
             echo area and moves nothing

## J2  Go back and forward through history

    from:    any surface reached through J1 or J3..J9
    end:     the same surface, byte-identical, point where it was
    today:   pending measurement
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
    today:   pending measurement
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
    today:   pending measurement
    target:  the find minibuffer, the first letters, enter: 2 keystrokes plus the
             letters
    rule:    the match list narrows per keystroke in O(log n); enter is the only
             confirmation
