# B-poll / B-retry / B-register — client loops audit

**Issue:** [#361](https://github.com/basic-automation/weftdb/issues/361)  
**Branch:** `b-poll/b-client-loops-audit`  
**Date:** 2026-11-22  

---

## Question

The `ROADMAP.md` backlog line (Phase 23, §7.1) reads:

> **B-poll / B-retry / B-register** Scheduled polling daemon, at-least-once retry buffer, runtime source registration

The three names each describe a background loop or persistent mechanism in the client/SDK stack. The label says **absent** and no roadmap checkbox has ever been ticked. This audit answers:

1. Where did these three names come from?
2. What exists today for each in `weft-client`, `clients/python`, and `weft-server`?
3. Is each genuinely missing, and does Weft need it?

---

## Method

Every citation in this document was produced by commands run at the branch head (`b-poll/b-client-loops-audit`, based on `origin/main` of 2026-11-22). No build was run; this is a structural grep-based audit.

Source code searched:
- `weft-client/src/` (Rust SDK, 4 files)
- `clients/python/weftdb/` (Python SDK, 4 files)
- `weft-server/src/` (server, shared daemon infrastructure)
- `weftdb/src/` (store library, transport/retry infra)

Keywords used (each with `--include='*.rs'` or `--include='*.py'`, case-insensitive except where noted): `poll`, `retry`, `register`, `backoff`, `heartbeat`, `daemon`, `schedule`, `cron`, `refresh`, `keepalive`, `long.poll`, `listen`, `watch`.

Documentation searched: `DECISIONS.md`, `docs/research/`, `docs/roadmap-notes.md`, `ROADMAP.md`, `roadmap-claim-findings.md`.  
Git history searched: `git log --all --oneline --grep='B-poll'`, `--grep='B-retry'`, `--grep='B-register'`.

---

## Origin of the three names

**No definition exists anywhere in this repository for any of the three names individually or collectively** — no `DECISIONS.md` entry, no research doc, no predecessor commit, no PR, and no issue body other than #361 itself.

The single source is the `ROADMAP.md` line at Phase 23, §7.1:

```
- [ ] **B-poll / B-retry / B-register** Scheduled polling daemon, at-least-once retry buffer, runtime source registration
```

This line was inherited from the predecessor (pre-split roadmap) and placed in Phase 23 (Online ingest) when the roadmap was restructured. The `roadmap-claim-findings.md` (line 115) rates it as "FINE — No falsifiable claim", meaning no one has ever asserted the feature exists — only that it is absent.

The three names have no separate issue definitions, no linked PRs, and no git log mentions (`git log --all --oneline --grep='B-poll'` returned zero results; same for `B-retry` and `B-register`). They are placeholder labels for a single envisioned behaviour: an online-ingest client that loops.

---

## What exists today

### Poll

| Location | What exists |
|---|---|
| `weft-client/src/` | **Nothing.** Zero hits for `poll`, `daemon`, `schedule`, `cron`, `refresh`, `listen`, or `watch`. |
| `clients/python/weftdb/` | **Nothing.** Same zero hits. |
| `weft-server/src/` | Server-side internal daemons exist (backup daemon, reconcile daemon, maintenance scheduler) but these are **server internal**, not client-facing. No long-poll or query-result handle polling. |
| `weftdb/src/` | No polling infrastructure. |

The client is entirely request-response: call a method, get a result, return. No background loop observes external sources or polls server-side query handles.

### Retry / backoff

| Location | What exists |
|---|---|
| `weft-client/src/client.rs:292-315` | Retry loop for `503 overloaded` on **GET** requests only. Respects `Retry-After` header; sleeps `min(retry_after, 30 s)`. `max_retries` defaults to 2. Writes are explicitly **not** retried by the client (caller uses idempotency keys). |
| `weft-client/src/client.rs:17` | `MAX_RETRY_WAIT` constant = 30 s. |
| `weft-client/src/client.rs:62-63` | `max_retries` doc: "Writes are never retried by the client: send them with an idempotency key and retry them yourself." |
| `weft-client/src/error.rs:24,74,110-116,169-170` | `Error::retry_after()` exposes the `Retry-After` value to callers. |
| `clients/python/weftdb/client.py:202-203` | Same `Overloaded` + `GET` retry in Python: `min(retry_after or 1.0, 30.0)` sleep, `max_retries` default 2. |
| `clients/python/weftdb/errors.py:28-38,70` | `WeftError.retry_after` field. |
| `weft-server/src/routes.rs:731,741,749` | Server sends `Retry-After: 1` on `503 overloaded` for query, write, and maintenance admission. |
| `weftdb/src/types/database/connection.rs:66-148` | Exponential backoff on Turso/SQLite connection transient errors (100 attempts, ~30 s total). |
| `weftdb/src/types/database/inputs.rs:84-137` | `queue_write_backoff()` — exponential backoff for MVCC conflict retries in ingest. |
| `weftdb/src/types/index_txn.rs:471-493,796` | `backoff()` function for index transaction retries. |

What exists is **simple `503` retry with `Retry-After` respect**. No exponential backoff in the client SDK (the exponential backoff lives in the store library, for database operations), no jitter, no circuit breaker at the client level, no persistent retry queue. The retry is per-request, not a "buffer".

### Register / source registration

| Location | What exists |
|---|---|
| `weft-client/src/` | **No runtime source registration.** Zero hits for `register` in the SDK (the `catalog()` method at `client.rs:531` is a read-only query, not a registration). |
| `clients/python/weftdb/` | **Nothing.** No mechanism to register a source, announce a client, or send a heartbeat. |
| `weft-server/src/` | Route registration (`routes.rs`), daemon registration (`daemons.rs`), audit task registration (`audit.rs`) — all server-internal and static/startup-scoped. The catalog endpoint (`/api/v1/storage/catalog`) is a read-only view of registered databases/subjects. No endpoint accepts a client registration or source registration. |

No mechanism exists for an external source or client to register itself with the server at runtime.

---

## Verdict per name

### B-poll — NOT NEEDED

WeftDB is a **push/pull store**, not an event broker or message broker. Query results are synchronous — the client sends a request and gets a complete answer. There are no query-result handles to poll, no cursor that needs periodic refresh, and no long-poll endpoint. A scheduled polling daemon would only make sense if the client were scraping an external data source and pushing it into WeftDB, which is application-specific logic (the user's own process) — not something the SDK should provide. Leaving this to the caller is the same choice as sibling SDKs (the Rust SDK has no polling daemon; the Python SDK has none either).

### B-retry — ALREADY PRESENT (for what the SDK exposes)

The client already retries `503 overloaded` GETs with server-respectful `Retry-After` sleep. Writes are deliberately left to the caller with idempotency keys — this is correct design: the server provides at-least-once semantics via `Idempotency-Key`, and only the caller knows whether a write is safe to retry.

What the line describes as "at-least-once retry buffer" — a persistent retry queue that survives process restarts and guarantees delivery — is not needed. The server guarantees at-least-once for replayed idempotency keys; a buffer on top is either (a) the same idempotency-key dispatch in a retry loop, which is what the caller already owns, or (b) a heavyweight persisted queue that belongs in an orchestration layer (if it belongs anywhere at all).

The simple improvement of adding exponential backoff with jitter to the client's existing `503` retry loop is not significant enough to file a separate issue — it would be a ~5-line change in a PR that touches this code for another reason.

### B-register — NOT NEEDED

WeftDB is not a scraping or aggregation gateway. It does not poll external sources; it accepts pushed data. Runtime source registration (where an external source announces itself and the server polls it) is a feature of metrics-scraping systems (Prometheus, etc.) and data-aggregation gateways. WeftDB's architecture is the opposite: the server is a passive receiver, and the client is a push/pull caller. No server endpoint accepts a source registration, and the SDK has no code to do it.

If a user wants to poll an external source and push into WeftDB, that is their application code. The SDK already provides all the HTTP routes needed (ingest, health checks, catalog) to build such a loop.

---

## Follow-ups filed

**None.** All three verdicts are "not needed" or "already present". No genuine gap warrants a follow-up issue.

---

## Phase label reconciliation

The audit found the following state:

| Source | Phase label | Evidence |
|---|---|---|
| `ROADMAP.md:205` | **Phase 23** (subsection "7.1 Online ingest") | Item lives under `## Phase 23 — Online ingest` |
| `docs/roadmap-notes.md:450` | **Phase 7** | `- **B-poll / B-retry / B-register** *(Phase 7 · absent)*` |
| Issue #361 title | **Phase 7** | `B-poll / B-retry / B-register (Phase 7 · absent)` |

The "Phase 7" label in both `docs/roadmap-notes.md` and the issue title is a carry-over from the predecessor roadmap's internal numbering. The item actually lives in Phase 23 (Online ingest). Both labels are corrected to Phase 23 in this slice.

---

## References

- `ROADMAP.md:205` — definitive placement
- `docs/roadmap-notes.md:450` — predecessor-derived backlog (corrected)
- `roadmap-claim-findings.md:115` — audit of the claim
- Sibling audits: B-ILP (#865, #867), B-conncache (#870), B-rest (#360, #877)
