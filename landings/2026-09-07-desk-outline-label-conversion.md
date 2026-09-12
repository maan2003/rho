The one-shot Desk conversion recovers the old outline as labels: the read-only
DeskSync proof changes notes 221→20, labels 18→29, agents 651→651, pages 7→0,
files 7→0, and Slack units 17→17. Its nine rules drop 68 archive stamps; mark
six archive folders and 227 descendants Done; convert 16 outside and flatten
eight archived headings into 12 new and three reused labels; drop seven project
files (all seven equal-name labels already had `Project`); collapse 93 item
notes onto 95 agents, comparing all 95 names with the registry at migration
time; discard two empty About candidates plus one other empty note; drop seven
bookmark headings and pages; clear 140 surviving non-label parents and all 380
agent snoozes while preserving 156 handled cursors; and remove one zero
creation time and two orphan bodies. This is one (O(2,381\text{ facts})) pass
at startup behind an atomic marker; a generated tempdir store asserts every
rule and that a second startup changes neither frontier, facts, nor bodies. The
orphaned first-conversion temporal parser is deleted with it.
