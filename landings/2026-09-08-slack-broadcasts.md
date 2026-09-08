# `@here` and `@channel` reach the channel

*eng-bgkw, 2026-09-08.*

`Model::encode` turns what the reader typed into what Slack counts: `@ada`
becomes `<@U1>`, `#design` becomes `<#C1|design>`. It resolved a name against
the user table, and the two channel-wide mentions are not users and are in no
member list, so that table could never answer for them. `@here` went out as
the four characters and reached nobody. Nothing said so: the composer's `@`
completion offers channel members, so there was no moment at which rho told
the reader these were not names it knew.

rho has always read one coming the other way. `block.rs` renders `<!here>`,
and `mentions_in` counts a broadcast as addressing the reader, which is what
earns a card from an `@here` somebody else sent. So rho could read one and
not send one.

Both now encode to `<!here>` and `<!channel>`, and the composer offers them
beside the people, from the same constant the wire form reads — so the
composer cannot offer something `encode` would not send. They keep the rules
every other mention has: the sigil must start a word and the word is the
whole of it, so `over@here.example` is an address and `@herero` is a name.
`@everyone` is left out, being meaningful in one channel only. An `@name`
that names nobody still goes out as text, which is a landed decision with a
test behind it, not a gap. What changes for the user: the two mentions that
tell a room something now tell it.
