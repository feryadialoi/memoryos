# ADR 0008: Insights, synthesized by the agent and kept honest by the store

Status: accepted

Facts record what happened. An agent that recalls the same scattered facts every session still has
to work out what they mean each time, and nothing carries that conclusion forward. MemoryOS
therefore adds insights: what several facts mean together, at three levels (context, understanding,
guidance), each citing at least two current sources of its own scope.

Synthesis needs a model, and ADR 0005 keeps every model call in the host. The agent writes insights,
and MemoryOS does the deterministic part: it chooses what to offer (a reflect batch of stale insights
and topics of uncited facts), checks every source on write, lowers trust to the weakest source,
quarantines guidance outside an agent's own scope, and marks an insight stale in the same
transaction as any change to its evidence. A conclusion is therefore always traceable, and never
outlives the facts it rests on.

Agents skip optional work, so reflect is started for them: the stop hook asks for it when a backlog
exists, and an opt-in nightly job runs the host CLI headless. Neither runs a model on the server.

Guidance changes how agents act, which makes it the most valuable target for poisoning. It is
printed as data labelled as learned from memory, below the user and `CLAUDE.md`, which keep the
rules people write. The cost is review of guidance in user and shared scopes, and staleness that is
strict on purpose: any changed source sends an insight back to the agent.
