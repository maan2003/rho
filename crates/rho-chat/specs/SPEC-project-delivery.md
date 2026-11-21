# SPEC-project-delivery: The host chooses what an idea starts

## Record justification

The device's durable outbox, the host's project settings and mail log, and the agent's inbox together own what an idea starts; none alone can guard against a lost receipt or a different device's settings.

A normal idea is logged once under its project and starts according to the host's settings, even if the sending window goes away. The standing agent is chosen once for the current place and mode; a settings change never repurposes its workset. An idea may explicitly name a runtime. A new agent uses it, or the project’s runtime if none was given. A saved project with no runtime reads `opus-high`. Changing the project’s runtime does not replace or reconfigure its standing agent, which without an override keeps its current runtime. A different runtime on the standing agent uses the same kind rule as `set runtime`. If refused, no mail is posted and the device retains the draft with the refusal in the echo. An already posted attempt can be sent again under the same client id without another agent or stop. Only a failed start is identified for retry, separate from thread replies. A retry is a new attempt in the original thread, uses the settings at the time of the reply, and gives a newly started agent the original idea before the retry words.

This implements [Project](../../../product/chat.md#concept-project) and [a refused start leaves mail](../../../product/agents.md#rule-a-refused-start-leaves-a-message). The explicit options path still names its own launch.
