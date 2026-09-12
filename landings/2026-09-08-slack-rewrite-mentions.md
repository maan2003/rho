# A rewrite tells the person it names

*eng-bgkw, 2026-09-08.*

`Model::encode` turns what the reader typed into what Slack counts — `@ada`
into `<@U1>`, which is the only form that makes the mention count for Ada.
`Session::send` has called it since it was written. `Session::edit_message`
never did: it passed the composer's text straight to `chat.update`, so a
mention added in a rewrite went out as the four characters `@ada` and reached
nobody. The line on screen read `@ada` either way, so nothing told the reader.

Adding the name you forgot is most of what a rewrite is for, which made this
the one way to type a mention in rho and have no one hear it. It is one line,
the same line `send` has, and it carries the channel link and the two
broadcasts with it. What changes for the user: editing a message to add
someone's name now tells them.
