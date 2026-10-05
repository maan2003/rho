`ste_checker` reads Markdown. It reports the text that does not agree with ASD-STE100
(Simplified Technical English).

![showcase](../.assets/showcase.png)

These are the rules that a program can apply: the approved wordlist, the part of speech
of each word, the length of each sentence, noun clusters, the passive voice, compound
tenses, -ing forms and contractions.

The standard has 53 rules. This program has 10 of them. A reader who understands the
text must apply the other 43. ASD and STEMG do not endorse this program, and it
certifies nothing. Use it as an aid, not as a gate.

A word can be approved in one part of speech and not in another. *Work* is an approved
noun and an unapproved verb. `ste_checker` tags each word before it reads the wordlist.
Thus "Do not work the lever" is a finding, but "The work is done" is not.
