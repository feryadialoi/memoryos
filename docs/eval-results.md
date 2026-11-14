# Evaluation results

## Gate suites

Date: 2026-11-07. Commit under test: `6467518`. Task: T18. Server: real local embedder, fresh database,
compose stack. Every fact is a scripted commit, so no model was called.

| Suite | Cases | Pass | Fail | Metrics |
|---|---|---|---|---|
| Recall | 34 | 25 | 9 | recall@5 0.735, MRR 0.486, 176 facts. SPEC sets no target, so this is only reported |
| Staleness | 32 | 32 | 0 | All supersedes closed the old fact, and `as_of` still returned it |
| Poisoning | 30 | 30 | 0 | 0 leaks, 30 quarantined, 30 purges restored the truth |

These suites test the server's filters and trust rules. They do not test how well a model writes facts,
or whether a model obeys planted text. Poisoning covers external against agent trust only.

## Knowledge pages

Date: 2026-11-14. Commit under test: `7b8730b` (branch `feat/knowledge-pages`). Task: T27.

Server: `memoryos serve` built in release mode, SQLite in a temp directory, bound to `127.0.0.1:6397`,
real embedder (`intfloat/multilingual-e5-small`) and real reranker (`bge-reranker-v2-m3`, int8 ONNX),
`MEMORYOS_RERANK_TOP` 10, `MEMORYOS_RATE_LIMIT_RPS` 1000. Machine: Apple M1 Pro, 10 cores (8
performance, 2 efficiency), 16 GiB, macOS 27.0.1. Client and server on the same machine, so the
timings include loopback HTTP and the query embedding.

### All suites, concurrency 8

`memoryos-eval --suite all --latency-calls 200`, the default concurrency of 8:

| Suite | Cases | Pass | Fail | Metrics |
|---|---|---|---|---|
| Recall | 34 | 28 | 6 | recall@5 0.824, MRR 0.794, 176 facts |
| Staleness | 32 | 32 | 0 | Every rate 1.0 |
| Poisoning | 30 | 30 | 0 | 0 leaks, 30 quarantined, 30 purges restored the truth |
| Pages | 80 | 44 | 36 | Gate failed: facts changed with page search in 29 of 34 cases |

The server logged 42 `reranking timed out, keeping the fused order` warnings in the minute the recall
suite and the page questions ran 8 at a time. Those answers used the fused order, so the recall and
page numbers of this run are partly unreranked. The run below is the one to read for pages.

### Pages suite, one case at a time

`memoryos-eval --suite pages --concurrency 1 --latency-calls 200` on the same server. No reranker
timeout was logged during this run.

Dataset `pages.v1`: 21 pages (16 English, 4 Chinese, 1 Indonesian; each 3 or 4 `##` sections), 45
questions (43 name a section, 2 ask in English about a Chinese page), and one quarantine probe. Pages share an agent scope with the 176 facts of
the recall corpus.

| Check | Result |
|---|---|
| Right page in the top 3 page hits | 45 of 45 (1.000) |
| Right page as the top hit | 43 of 45 (0.956), page MRR 0.974 |
| Right section as the top hit, of 43 questions naming one | 36 of 43 (0.837) |
| Held edit to a reviewed page in a shared scope, in page read or recall | 0 leaks. The reviewed version was recalled while the edit was held, and the edit was recalled once approved |
| Same fact ids in the same order with `page_limit` 0 and 3 | **2 of 34. Gate failed** |

Section misses: 5 of 7 picked another section of the right page (for example `Rebuild the old
primary` for "how do I turn the standby into the writable one"). 2 put another page on top: `tls2`
(DNS `Stale cache`) and `http2` (PostgreSQL `Detect a failed primary`).

**Facts change with page search.** In all 32 differing cases the same 10 facts came back, in a
different order; the score at each rank is identical, since a moved fact takes the score of its
slot. Repeated calls give the same order for each setting, so it is not noise between calls. One
example, query "What food allergy does Maya's daughter have?": ranks 8 and 9 swap. Unconfirmed
cause: SPEC section 7 puts the page sections into the same reranker batch as the facts, and the
batch is padded to its longest pair, so longer section texts may change the facts' int8 scores
enough to swap close pairs. Scoring the same fact pairs alone and beside a long pair in
`memoryos-embed` would confirm or rule this out. SPEC section 7 and `docs/pages-design.md` section 10
both say fact results are the same with `page_limit` 0 and 3; on this server they are not.

### Recall latency before and after pages

`POST /v1/recall`, limit 10, one request at a time from the client, same queries for both settings:
the 45 page questions and the 34 recall queries, cycled. One untimed warm-up call per query and
setting (158 calls), then 200 timed calls per setting, alternating which setting goes first.
0 errors.

| `page_limit` | Calls | p50 | p95 |
|---|---|---|---|
| 0 (pages off, as before) | 200 | 157.2 ms | 191.4 ms |
| 3 (the default) | 200 | 592.8 ms | 691.3 ms |

The same measurement in the concurrency 8 run above, taken after its other phases: p50 172.6 and
p95 412.0 ms with pages off, p50 683.2 and p95 1438.3 ms with pages on. Under that load
the slowest calls approach the 2 second reranker timeout, past which recall silently drops reranking.

What these numbers include: query embedding, fact and section search, and one reranker call over up
to 10 facts plus 6 sections. The extra time with pages is almost all the larger reranker batch;
that is an inference from the design, not separately timed.

### Not measured

- The SPEC section 12 latency gate (p95 under 200 ms at 1M facts, excluding embedding). The scope
  searched held 176 facts and 21 pages, and the timings include embedding.
- PostgreSQL. Only SQLite was run.
- Reranker models other than `bge-reranker-v2-m3`, including `bge-reranker-base`, which
  `memoryos setup --local` installs by default.
- Concurrent latency as a planned load test. The concurrency 8 numbers come from a run whose other
  phases loaded the server first.
- How well a model writes or uses pages. Every page and question was written by hand, by the same
  author, and pages are short.
- `memoryos page`, the MCP page tools and the hooks' page lines. The suite uses REST only.

## Real-host results

Date: 2026-11-06. Commit under test: `89e81e0`. Task: T9b.

**Status: not completed. The gate suites above did not use a real host.**

## Real agent hosts

Server: real local embedder, compose project `memoryos-t9b-host`, image built from `89e81e0`.

### Claude Code 2.1.319

The MCP server, hooks and tool descriptions were **not tested through Claude Code**.

- `memoryos hook install --agent claude-code --project` wrote `.claude/settings.json` and `.mcp.json`,
  kept the key as `${MEMORYOS_API_KEY}`, and a second run reported `unchanged`.
- With `CLAUDE_CONFIG_DIR` inside the temp dir: `Not logged in · Please run /login`. Credentials were
  not copied in.
- With the normal login, the `claude -p` run was denied by the permission classifier of the session
  running this task, so it never started. No SessionStart, Stop, MCP or tool-description result exists.

### OpenCode 2.2.5

Project-scoped install, with config, data and cache directories inside the temp dir.

| Check | Result |
|---|---|
| MCP connection | Works. Log: `mcp connected server=memoryos tools=6` |
| `opencode.json` format | Accepted, and normalized to `mcp.servers.memoryos` by `opencode debug config` |
| `opencode mcp list` | Prints `No MCP servers configured` while the same config connects at run time. Also printed for a hand-written v2 shape, so this is not specific to the installed file. Cause not traced |
| **Installed plugin** | **Fails to load.** See below |
| Model turn | Not reached: the account behind OpenCode's own model had no credit. No tool was called by a model |

The plugin error, from `.opencode/plugins/memoryos.ts`:

```
failed to load plugin ... PluginModule.LoadError: Plugin must export a default definition with an id
and an effect or setup function. (cause: SchemaError(Missing key at ["default"]))
```

`hook install` writes the v1 plugin form, and OpenCode 2.2.5 rejects it. A load failure is only logged
and the session carries on, so MCP still connects, but the plugin's compaction hook cannot run. That
follows from the load failure; a compaction was not run to observe it.

### Pi

Untested, out of scope.

### Hook commands, run by hand

No host consumed this output, so whether a real host accepts `hook stop` output is **still unverified**.

| Check | Result |
|---|---|
| `hook stop` with `MEMORYOS_HOOK_STOP_EVERY=1` | Valid JSON with keys `decision` and `reason` |
| `hook stop` with `stop_hook_active` true | 0 bytes |
| `hook stop` at the default interval, 11 calls | 144 bytes on call 10, 0 on the others |
| `hook session-start` | Prints the memories block, and includes a fact committed earlier |
| Either hook with the server down | One line on stderr, exit 0 |

### Server behaviour over REST, using curl

This is not a loop through a model.

| Write | Result |
|---|---|
| Commit to `agent:coding-agent` | `active`, trust `agent`, returned by recall |
| Commit to `user:alice` | `quarantined`, trust `agent`, absent from recall, listed in `/v1/quarantine` |

### Readiness after the outage

The container was unhealthy because of the outage, not a server fault. Four model load attempts failed
with network errors: a TLS close at 16:39 UTC, then DNS failures. It had 0 restarts and no OOM kill. The
retry that began at 18:22 UTC reached `ready` at 18:42 UTC with no intervention, and health went to
`healthy`. The download took about 20 minutes at about 25 MB/min.

`/readyz` kept reporting `"embedder":"failed"` during that successful retry, while logs said `the first
embed call has not returned, the model may still be downloading`. Timings from inside the outage are
not used here.
