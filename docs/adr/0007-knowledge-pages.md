# ADR 0007: Knowledge pages as a second directly edited record

Status: accepted

Agents need knowledge that has the shape of a page: how a system works, runbooks, one page per
feature. Facts hold one claim each, and the embedder reads at most 512 tokens, so a runbook squeezed
into a fact is cut off or loses its steps. Blocks are page-sized but belong to one agent, are not
searchable, and are not shared.

MemoryOS therefore adds pages: markdown documents in a scope, versioned like blocks, split into
sections by heading for search, and returned by `recall` as a separate list of page hits next to
facts. Like blocks, pages are edited directly and are not derived from episodes, so ADR 0002's
exception for blocks covers them and facts stay the only graph edges.

Pages stay inside MemoryOS rather than a separate service because they need the same tenants, API
keys, scope patterns, trust gate, embedder, reranker, hooks and setup. A second service would
duplicate each of these and load the models a second time.

Pages follow ADR 0004: a write that facts would quarantine creates a pending version, and readers
keep the last approved version until an admin approves it. The cost is review of agent edits to
shared and user pages.
