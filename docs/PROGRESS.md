# MemoryOS progress

Only the lead edits this file. A task is `done` only with proof from a command or test that was
actually run. States: `todo`, `doing`, `done`, `blocked`, `removed`.

| Task | State | Proof |
|---|---|---|
| T0 Spec, core, schema, OpenAPI | done | clippy clean, 2 core tests pass, migration applied on pgvector:pg17, psql check: no tenant sees 0 rows, cross-tenant insert rejected by RLS, app role denied on `jobs` |
| T1 Postgres store | done | Lead run on `develop`: 4 unit and 51 integration tests pass against real Postgres. Open risk: common-word text search in a 1M-fact scope is estimated at about 450 ms, over the latency gate; T9 must measure it |
| T2 LLM and embedding clients | removed | Removed by T15, except `LocalEmbedder` (T11). In git history |
| T3 Eval harness | done | Lead run on `develop`: 51 unit and 6 WireMock tests pass. Not yet run against a real server |
| T4 Ingest and worker | removed | Worker and extraction removed by T15. `HostCommitter`, the trust gate and entity resolution stay |
| T5 Hybrid retrieval | done | Lead run on merged `develop`: 18 unit and 22 integration tests pass |
| T6 Agent runtime | removed | Removed by T15. In git history |
| T11 Local embedder | done | Lead run: 54 llm unit tests pass. Real-model test (6 pass) reported by the agent, not re-run by lead. Needs at least 1 GiB memory and glibc 2.38+ |
| T13 Hardening after Wave 2 | done | Lead run on merged `develop`: 387 tests pass, fmt and clippy clean |
| T14 Purge fixes | merged | Agent reports 3 unit and 73 integration store tests pass, 35 unit and 44 integration ingest tests pass. Lead workspace run at T7 merge passes |
| T7 Server, MCP, SDK | done | Lead workspace run on `develop`: 506 tests pass, fmt and clippy clean. MCP checked against spec text and tests, not a real host. No live providers |
| T12 CLI commands and hooks | done | Agent reports 96 unit and 25 integration tests pass. Real OpenCode connected over MCP; real Claude Code not run |
| T8 Deploy, CI, docs | done | Agent reports clean Docker build, compose install and restore drill, run before host-only removal |
| T9 Integration and gates | doing | Load test stopped early by the user; see `docs/loadtest-results.md` |
| T10 Independent review | done | 21 findings from code reading; fixes merged through T14, T15 and T16 |
| T15 Host mode only | done | Merged at 81641f8 |
| T16 Review fixes | done | Merged; agent reports store, ingest and server suites pass |
| T17 One-command setup | done | Merged at 1cc275a. Real OpenCode 2.2.5 and Pi 1.4.0 connect with 6 tools. Full first run through `setup` completed on commit e21543e in a temp home: image build, model download of about 29 minutes at 220 to 470 KB/s, ready and healthy |
| T18 Eval without a model | done | Merged at 6467518. Lead run against a real server; see `docs/eval-results.md` |
| T19 Memory page | done | `memoryos ui`, `GET /v1/scopes`, `GET /v1/facts`. Lead on e21543e: 548 tests pass, fmt and clippy clean. Real browser with sample data: all views, approve, phone width, light and dark. Real `setup` and model: Ask and Review work, no console errors. For "what database do we use?" over 5 facts, the Postgres fact ranked 3rd |
| T20 Pages contracts | done | Lead on `feat/knowledge-pages`: fmt and clippy (all targets, all features) clean, 463 workspace tests pass, store integration 77 on Postgres 0004 and 36 on SQLite 0002 pass, SQLite cascade check removes versions, sections, vectors and FTS rows. axum 0.8 accepts the route set |
| T21 Pages store | done | Merged f9850ba, fix 8bae2aa (a page with no current version takes a new first version). Lead on 7b8730b: store 88 on Postgres and 47 on SQLite pass |
| T22 Page splitter and writer | done | Merged 3a0855a. Agent reports 63 ingest unit tests; lead workspace run on 7b8730b passes. SPEC now says limits apply to the masked text |
| T23 Page search in recall | done | 09142a0, 6ac04da. Lead: 44 unit tests, page recall scenario passes on Postgres (27) and SQLite (4). One reranker call per recall; fact results equal with page_limit 0 and 3 in tests with a fake reranker |
| T24 Pages REST, MCP, SDK | done | Merged; fix 7b8730b (quarantine patterns cover scopes with only pending pages). Lead on 7b8730b: server 76 on Postgres including both contract tests, 12 on SQLite |
| T25 Pages in hooks and CLI | done | Merged 0408e71; fix: recall retries without page_limit on a server from before pages. Lead on 7b8730b: hooks integration 36 pass. Real Claude Code session not run |
| T26 Pages on the review page | done | Merged 59516c1; fix 0d0cbd4 (scope list includes scopes with only pending pages). Agent's Playwright check on a SQLite server: pages view, plain-text bodies with planted HTML shown as text, diff review, approve, 409 on a stale edit, light, dark, 390 px. Lead viewed two screenshots. Not checked in a browser: Postgres, an older server |
| T27 Pages eval, latency, docs | done | Merged 6191729. Found that one shared reranker batch changed fact order (32 of 34) and cost 3.8x at p50; the lead split the calls. Lead rerun with real models on SQLite: facts unchanged 34 of 34, page top 3 45 of 45, all four suites pass at concurrency 8, recall p50 147 ms without pages and 384 ms with. See docs/eval-results.md. Not measured: Postgres, other rerankers, 1M facts, a model writing pages |
| T30 Insights contracts | done | Core types and store methods (default bodies return internal until T31), Postgres 0006 and SQLite 0003, SPEC section 8 Insights, OpenAPI paths (listed as not yet served in the contract test until T34), ADR 0008. Recall's `insight_limit` and `insights` fields move to OpenAPI with T33. Lead run: clippy clean (all targets, all features), core 10 tests (4 new), store integration with both migrations applied 89 Postgres and 47 SQLite, contract 2, workspace tests pass |
| T31 Insights store | done | Merged b0fe4c3, lead fix (Store dispatched none of the insight methods, hidden by the trait's default bodies, which are now removed; confirm takes the writer's trust). Agent: store 108 Postgres, 66 SQLite, workspace 575. Lead after merging T31 and T33: store 108 Postgres and 66 SQLite, retrieve 29 Postgres and 6 SQLite, workspace tests pass |
| T32 Reflect batch | done | Merged at 322c62c. Agent: reflect 10 on Postgres and 10 on SQLite, store suites and workspace pass; a mutation check showed the tests fail without the status filter or tenant predicate |
| T33 Insights in recall | done | Merged. Insights fused with the shared `fusion::fuse_by`, never reranked; a failing insight search leaves only that list empty. Agent: retrieve 50 unit, 29 Postgres, 6 SQLite; fact ids and score bits equal with insight_limit 0 and 3 |
| T34 Insights REST, MCP, SDK | done | Merged 911ade3. Statement masked with the ingest `redact` (now exported), writer trust is the key's max_trust capped at agent. Agent: server 86 Postgres integration and 21 SQLite, SDK 21, workspace 590; both contract tests cover every operation |
| T35 Insights in hooks and CLI | done | Merged f5557c3; lead fix 413cc5e (the MCP proxy fills a missing scope for the insight tools). Agent: hooks 283 unit and 43 integration, workspace 632. Nightly schedule tested with fake launchctl, systemctl and claude in a temp HOME; host command forms checked against each CLI's --help only |
| T36 Insights on the review page | done | Merged 9f4fd14. Agent's Playwright check on a throwaway SQLite server: every level, status and stale change, approve, reject, retire, planted HTML shown as text, light, dark, 390 px, no console errors. Lead viewed the stale drawer screenshot |
| Map rework | done | Merged 2e8dcee. Live d3-force layout on a canvas. Agent measured in headless Chromium: settles in 1.7 s, no frame over 16.7 ms at 34 real nodes and at 300 and 450 synthetic nodes |
| T37 Insights eval, live run, README | done | Merged. Insights suite: 23 scripted cases, 131 checks, plus 34 fact-equality cases. Agent's live run on SQLite with the real e5-small embedder and bge-reranker-v2-m3: recall 28/34 (same as before), staleness 32/32, poisoning 30/30, pages 73/80 (74 before; 53 reranker timeouts logged at concurrency 8), insights 57/57, all gates pass. Recall p50 / p95 156.2 / 194.8 ms with insight_limit 0 and 157.8 / 198.5 ms with 3 |

## Production gates (SPEC section 12)

| Gate | State | Proof |
|---|---|---|
| Static checks | done | Lead on 6467518: fmt and clippy clean. `cargo deny` reported ok by T15, not re-run since |
| Recall latency | not met | 1M facts in one scope: recall p95 385 ms at 1 client on a busy 3.8 GiB VM. Small scopes not measured. Stopped by the user |
| Staleness | done | `memoryos-eval` on a real server: 32 of 32, scripted commits with no model |
| Poisoning | done | `memoryos-eval` on a real server: 30 of 30, 0 leaks, 30 purges restored the truth. Covers external against agent trust with scripted commits, not whether a model obeys planted text |
| Isolation | done | Lead on 6467518: 535 tests pass, including two-tenant integration tests on real Postgres. The restored server gave another tenant no facts |
| Restore | done | Lead drill on 6467518: dump and restore gave equal counts (716 facts, 392 episodes, 9 keys) and 8 policies, and a key made before the dump recalled the same 5 facts |
| Ready to use | partial | `setup` works end to end with real OpenCode and Pi. Not run: a model calling the tools in any agent, Claude Code, Linux, Windows |

## Contract changes

Host mode only, by the user's decision (T15):

- MemoryOS never calls an LLM. Removed: `crates/agent`, the LLM clients, `Llm` and its types, `Job`,
  sessions, the worker, `POST /v1/episodes`, `/v1/sessions/*`, `POST /v1/agents/{id}/sessions`, MCP
  `remember`, `memoryos worker`, `--no-worker`, and the mode, provider and embedder settings.
- `memoryos-llm` is now `memoryos-embed`. `apply_extraction` is `apply_commit`, and
  `claim_unqueued_episode` is `claim_episode`, the only way to store an episode. `RecallResponse` has
  only `facts`, and `/readyz` no longer reports `not_required`.
- Approving now also requires `max_trust` to reach the trust of the `pending_supersede_of` target.
- `entity_alias_sources` moved into `0001_init.sql`, the only migration.
- The migration lost `jobs`, `sessions`, `session_messages`, `enqueue_job`,
  `search_pending_episode_ids`, `episodes.content_tsv` and the episode search indexes. `episodes` is
  back to `FORCE ROW LEVEL SECURITY`. `episodes.processed_at` stays: it makes a replayed commit a no-op.
- The Recovery gate is gone, because no job exists to recover. Entries below describe the removed
  design where they mention jobs, workers or Anthropic.

After Wave 1, from agent findings:

- `Fact.pending_supersede_of` added; `NewEpisode.metadata` defaults to `{}`.
- `CompletionResponse.stop_reason` added.
- Job methods fenced by worker and attempts; `renew_job` added.
- `agent` trust facts quarantined in `user:*` and `shared:*` scopes; pending episodes limited to
  `user` and `system` trust.
- Text search moved to `SECURITY DEFINER` functions because GIN is unusable under RLS.
- Indexes added on `superseded_by` and `pending_supersede_of`.
- Default Anthropic model changed to `claude-haiku-5-5` for forced tool calls. Not verified against
  the live API because no Anthropic key is available.
- Work merges to `develop`, not `main`.

No paid provider calls, by the user's decision:

- Staleness and poisoning gates run through `memoryos-eval --mode host`, with no LLM.
- Worker mode stays available but is unproven against real models.

Host mode, chosen by the user:

- Host mode is the default (ADR 0005, SPEC section 14). `insert_unqueued_episode` added.
- Embedding width changed from 1536 to 384 (ADR 0003).

After Wave 2, from T13:

- `MemoryStore` gains `find_entity`, `facts_for_episode`, `release_job`, and `neighbors` gains
  `edge_limit`. Core exports `normalize_name` and `source_requires_quarantine`.
- Migration gains `entities.alias_keys`, `facts_subject_recent_idx` and `facts_object_recent_idx`.
- Test Postgres runs on tmpfs, because each run was leaking an anonymous Docker volume.
