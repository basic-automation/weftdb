# Limits and overload evidence (ROB-24)

The 1.0 release criteria box **7.6 Quotas/limits** reads: "request
size, query timeout, memory, GPU memory". This document is the
evidence for that box: every limit the server enforces, the knob
that sets it, the refusal a client sees, and the test that proves
it. It also records the evidence for the **panic audit** box
(no `.unwrap()`/`.expect()` reachable from a request or ingest
path; failures surface as typed errors), which ticks with
ROB-2/3/4 plus this slice.

The overload soak itself is **not** this document's evidence: the
soak runs belong to the SOA lane (`scripts/crash-soak.sh`, SOA-17;
the 24 h acceptance run, SOA-18). What this slice proves, in
process and repeatably, is that the limits hold under load — the
bounded battery in `weft-server/tests/rob24_limits_overload.rs`
drives the production router past its ceilings and asserts typed
refusals, answering probes, and survival. A clone re-runs it with
`cargo test -p weft-server --test rob24_limits_overload`.

## The limits

| Limit | Knob (default) | Refusal | Proved by |
|---|---|---|---|
| One ingest body | `WEFT_MAX_INGEST_BYTES` (256 MiB) | `413 payload_too_large` with the limit | `ingest.rs`: `a_body_over_the_configured_cap_is_payload_too_large` |
| Decoded ingest size | `WEFT_MAX_INGEST_DECODED_BYTES` (1 GiB) | `413 payload_too_large` with the limit | `ingest.rs`: `a_decoded_column_set_over_the_byte_cap_is_payload_too_large` |
| Rows per ingest | `WEFT_MAX_INGEST_ROWS` (10,000,000) | `413 too_many_rows` with the limit | `ingest.rs`: `a_batch_over_the_row_cap_is_too_many_rows`; battery: `read_ceiling_battery_refuses_each_shape_and_the_server_survives` |
| Ingest bytes in flight | `WEFT_INGEST_MAX_INFLIGHT_BYTES` (256 MiB) | `503 overloaded` + `Retry-After: 1` | `ingest.rs`: `admission_refuses_an_ingest_over_the_inflight_budget`; battery: `two_ingests_past_the_inflight_budget_shed_the_second_and_the_first_still_seals` (concurrent half) |
| Queries in flight | `WEFT_QUERY_MAX_INFLIGHT` (128) | `503 overloaded` + `Retry-After: 1` | `routes.rs`: `query_admission_sheds_beyond_the_cap`; battery: `query_class_sheds_on_the_production_router_and_probes_still_answer` (production wiring) |
| Maintenance concurrency | (exclusive by definition, no knob) | `409 busy` + `Retry-After: 1` | `routes.rs`: `maintenance_admission_is_exclusive_at_the_gate`; `maintenance_busy.rs`: `the_maintenance_endpoints_answer_409_while_another_operation_holds_the_aspect` |
| Maintenance output plan | `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` (unset: unbudgeted) | `StoreError::OverBudget` raised before the group is read; nothing is written, the aspect keeps the segments it had. The per-aspect maintenance routes answer **`503 over_budget`** with `details.budget_bytes` and `details.planned_bytes` and **no `Retry-After`** — the refusal is identical until an operator raises the ceiling, so a delay would only send the caller back into it. On the store-wide sweep it is not a status at all: the aspect is listed in `failed` as `{aspect, error}` and the sweep still answers `200`. Before this code existed the classifier's fall-through made a refused plan look like a damaged store (`503 incompatible_store`). | `segment_store/maintenance_budget_tests.rs`: `a_group_over_the_budget_is_refused_before_anything_is_read`, `a_refusal_is_not_a_lost_merge_the_same_pass_runs_once_the_budget_rises`, `the_knob_is_a_positive_byte_count_and_nothing_else` |
| Maintenance output size | `WEFT_MAINTENANCE_MAX_SEGMENT_BYTES` (unset: no re-cutting) | Not a refusal: a fold whose output is estimated above the ceiling re-cuts that output at timestamp boundaries, at most one frame per segment it replaced, and writes those. The row set is unchanged; the segment count is higher and each frame is smaller. A fold of one segment is not cut. | `segment_store/maintenance_budget_tests.rs`: `a_fold_over_the_ceiling_divides_its_output_rather_than_writing_one_frame`, `the_same_fold_writes_one_frame_when_the_ceiling_is_off`, `a_ceiling_far_above_the_fold_writes_what_an_unbounded_fold_writes`, `the_cuts_divide_the_row_range_and_never_split_a_run_of_equal_timestamps`, `a_fold_is_never_cut_into_more_outputs_than_the_segments_it_replaces` |in
| Page rows | `WEFT_MAX_PAGE_ROWS` (100,000) | `422 result_too_large` with the limit | `read_limits.rs`: `a_page_over_the_ceiling_is_refused_and_one_at_it_is_served`; battery |
| Read rows (window) | `WEFT_MAX_READ_ROWS` (5,000,000) | `422 result_too_large` with the limit | `read_limits.rs`: `a_window_over_the_row_ceiling_is_refused_however_it_is_asked_for`; battery |
| Downsample buckets | `WEFT_MAX_DOWNSAMPLE_BUCKETS` (1,000,000) | `422 result_too_large` with the limit | `read_limits.rs`: `a_downsample_grid_over_the_bucket_ceiling_is_refused`; battery |
| `at-multi` instants | `WEFT_MAX_AT_MULTI_INSTANTS` (10,000) | `422 result_too_large` with the limit | `read_limits.rs`: `an_at_multi_batch_over_the_cap_is_refused_and_a_smaller_one_is_served`; battery |
| Read wait | `WEFT_READ_TIMEOUT_SECS` (30 s) | `504 query_timeout` | `read_limits.rs`: `a_read_still_waiting_when_its_deadline_passes_is_refused_with_504` (a genuinely waiting read; the zero-deadline wire case is deliberately not asserted — see the ROB-12 entry in `DECISIONS.md`) |
| Interpolate points | `WEFT_MAX_INTERPOLATE_POINTS` (10,000,000) | `422 result_too_large` with the limit | `interpolate.rs`: `oversized_grid_is_refused_with_the_cap` |
| Series per aspect | `WEFT_MAX_SERIES_PER_ASPECT` (100,000) | `400 series_limit` | `ingest.rs`: `series_limit_is_400_series_limit` |
| Connections | `WEFT_MAX_CONNECTIONS` (half of `RLIMIT_NOFILE`, 16..=65,536) | beyond the cap, waits in the listen backlog unread until a slot frees — no refusal, by design (SEC-4) | `serve_path.rs`: `the_connection_cap_leaves_excess_connections_unserved_until_a_slot_frees` |
| Disk reserve | `WEFT_DISK_RESERVE_BYTES` (256 MiB; `0` off) | `507 insufficient_storage`; `/ready` stays `200` with `disk_refusing` | `ready_identity.rs`: `ready_reports_a_refusing_disk_guard_and_stays_ready`; battery: `disk_guard_refuses_writes_with_507_and_reads_keep_working` |
| Body arrival | `WEFT_INGEST_BODY_TIMEOUT_SECS` (300 s) | `408 body_timeout` | `ingest.rs` two-phase extractor tests (ROB-18) |
| Header / keep-alive / write-stall deadlines | `WEFT_HEADER_READ_TIMEOUT_SECS`, `WEFT_KEEP_ALIVE_TIMEOUT_SECS`, `WEFT_WRITE_STALL_TIMEOUT_SECS` | connection closed at the deadline | `serve_path.rs`: `a_peer_that_never_finishes_its_headers_is_cut_at_the_header_deadline`, `an_idle_read_times_out`, `a_stalled_write_times_out` |

The probes are never shed: with a zero query cap every query is
`503` and `/health` and `/ready` still answer `200`
(`routes.rs`: `probe_admission_never_sheds`; battery). A shed probe
would read as a dead process and start the restart machinery, which
is the most expensive answer available.

## What the battery adds

The per-limit tests above pin each ceiling where it lives. The
battery in `weft-server/tests/rob24_limits_overload.rs` adds the
two things no single-ceiling test can show:

1. **The production wiring.** The gate tests run on a synthetic
   registry with slow fixtures; the battery runs the same shedding
   against `app_with_state` over a real store, proving the registry
   the binary serves carries the admission.
2. **The concurrent and combined behaviour.** Two ingests whose
   declared bodies sum past the in-flight budget shed exactly one
   (the hold is a streamed body the test releases, so the refusal
   depends on the declared lengths, not on timing); the read
   ceilings, the ingest row cap and the disk guard are refused in
   one run; and the run ends with the server serving a normal read
   and a normal ingest again.

## Known gaps (not claimed)

- **Maintenance budgets (ROB-20) are partly closed.** Two cuts are in. The
  disk guard now covers maintenance swap outputs before a byte is written
  (`seal.rs:swap_plans`, G2). And the first byte budget (#758, G1) refuses an
  overlap-reconcile group whose segments total past
  `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` with `StoreError::OverBudget` before the
  group is read, which is the memory spike the budget exists to bound. What
  stays open is the rest of the 7.5 chain: the k-way merge that would fold a
  group without materialising it (G3), the re-split of oversized frames (G4), a
  time budget for a pass (G5), and the maintenance daemon's settings surface
  (G6). `squash` and `compact` remain unbudgeted, so the budget guards
  overlap-reconcile only, and a refused pass is still not a distinct HTTP
  answer (#764). This document does not claim the maintenance class is bounded
  by anything else.
- **Resident memory has no budget (ROB-14 skipped).** Per Justin's
  2026-11-07 call, ROB-14's memory budget is skipped, not
  implemented. Host memory is bounded indirectly — per-request row
  and byte ceilings, the in-flight budgets, and the query cap — but
  there is no resident-memory ceiling, and none is claimed here.
- **GPU memory has no server-side cap.** The `weft-bench`
  `--gpu-memory` workload measures pool stability and eviction, and
  the interpolate points cap bounds the largest grid a request can
  ask the GPU to build, but no knob caps GPU memory. The 7.6
  "GPU memory" element is therefore evidenced only by those two
  bounds, and a GPU-memory ceiling would be a later slice.
- **The soak numbers come later.** Bounded p99, loss bounds and the
  24 h acceptance evidence are SOA-17/SOA-18's runs, not this
  battery's. This document must not be cited as soak evidence.

## Panic audit (:799) disposition

The panic-audit box ticks with ROB-2/3/4 plus this slice. The
handling side is ROB-2's engine-origin abort, ROB-3's panic
mapping and daemon supervision, and ROB-4's enforced no-panic lint
in `weft-arrow` (with its `Result` API). This slice is the
behavioural side: under overload — every ceiling above driven past
its limit in one run — the server answers typed errors
(`422`/`413`/`503`/`507`/`409` with the envelope code and the
limit), the probes keep answering, and the server keeps serving
afterwards. No request in the battery produces a `500` from a
panic, and no handler panic is reachable through a limit refusal.

What this does **not** prove, and does not claim: there is no
workspace-wide ban on `.unwrap()`/`.expect()` — `weft-server`
alone carries over a thousand call sites, most of them in tests,
startup, and daemon code outside the request path, and ROB-4's
lint covers `weft-arrow` only. A static audit of every reachable
call site would be a separate slice; the box is ticked on the
handling plus this behavioural evidence, and the W11 audit decides
the flip.
