## goal.md

Topics as grouping of agents
agents with metadata - including provider metadata, caller is meant to pass initial provider metadata, some of metadata is changable at runtime. prompt cache key will part of provider
workflows we will deal with later, for now lets just say agents have metadata which is persisted in db, we can make abstractions on top within the same db transaction, but it is for future thinking and if we don't like the model we can change it like always


- remote ui
- better thread rendering and more info about running tools
  - for shell: maybe last few lines of output?
  - duration
  - for apply patch: the diff after run was completed, maybe a streaming diff?
- async shell
- subagents
- topics
- integration in tau-gui
