# An edit starts from the words the reader typed

*eng-bgkw, 2026-09-08.*

`ConversationView::begin_edit` filled the composer with `message.text`, which
is Slack's wire string straight from `api::parse_message`. The transcript
draws the rendered form, so a message reads `@ada` on screen and arrived in
the composer as `<@U1>`. The comment above that line already said the
composer holds the reader's own words; it did not, and the gap was worse than
cosmetic — a reader who tidied it up by typing the name they could see,
`@Ada Lovelace`, sent prose, because `encode` stops a handle at the space and
finds nobody.

`Model::decode` is now `encode`'s inverse, beside it. The existing renderer,
`block::render_mrkdwn`, was the wrong tool twice: it resolves a user to their
display name, which `encode` cannot match, and it renders
`<https://x.example|docs>` down to `docs`, which would send the rewrite back
with the address gone. So the rule is that an escape is rewritten only when
`encode` would name the same person or channel coming back — compared by id,
not by bytes, since Slack writes both `<#C1>` and `<#C1|the-old-name>` for
one channel. Everything else is left exactly as the wire wrote it: a link, a
subteam, an id the roster does not know.

That means the reader editing a message with a link in it sees the link in
its wire form. Lossless over pretty was the call: there is no form the reader
could type that puts an address back, so rendering it would destroy it. What
changes for the user: rewriting a message that mentions someone starts from
their name, and sends the same mention it started as.
