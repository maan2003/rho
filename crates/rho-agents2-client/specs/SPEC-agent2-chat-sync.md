# SPEC-agent2-chat-sync: Agent2 chat across a host

## Record justification

The host's durable log projection, the client protocol, and the GUI's chat buffer share this contract, so none can own it locally without also governing the other two.

The host log is primary. A connected client receives each agent's chat projection initially and then every appended message or status with its original log position. Reconnecting replaces the client's snapshot; neither notebook code, tool output, reports, nor reasoning crosses the chat protocol. An agent's explicit archive mutes it, and a later human message revives a fresh notebook while preserving chat history.

Creating an agent records its work directory, model, and effort alongside its durable log under the host state directory. Its strong agent ID identifies both the chat and the log across host restarts. Agent-to-agent messages are logged at their sender and delivered into the recipient's chat and wake queue when that recipient belongs to the same host manager.
