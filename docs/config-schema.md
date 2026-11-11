# Configuration schema

schema_version: 2

WeftDB is configured primarily through environment variables, registered in a single
source-of-truth registry (`weft-server/src/settings.rs`, 1.0 plan PAC-5a). Every variable
any crate in the workspace reads is listed there; an unrecognised `WEFT_*` name (a typo such as
`WEFT_BACKUP_DIRS`) or an unparseable value stops `weft-server` at startup with exit status
`78`. A variable the registry knows but a reader cannot parse also stops the server, unless
the reader's definition says the value is advisory (e.g. `WEFT_SEGMENT_PARTIAL_TIERS` drops
unparseable entries).

The registry assigns each variable a **tier** that says how much it is promised.

## How this version changes

| schema_version | Change |
|----------------|--------|
| 1 | First versioned schema. Covers every variable the registry names (105 variables: 13 Stable, 43 Tuning, 8 Experimental, 41 Test). The machine-readable version lives in `config/schema.toml` and is authoritative; this document's `schema_version:` line must match it. The checker `scripts/config/check_schema.py` validates both against the registry at `weft-server/src/settings.rs`. |
| 2 | One new Tuning variable (`WEFT_DISK_RESERVE_BYTES`, default 256 MiB, opt-in). Nothing renamed or removed. Three further Tuning variables were added after v2 with no version bump, following the precedent that adding a variable does not change the schema's structure: `WEFT_INGEST_BODY_TIMEOUT_SECS` (300 s ingest body deadline), `WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS` (300 s breaker cooldown) and `WEFT_MAINTENANCE_BREAKER_FAILURES` (3, breaker trip threshold). 105 variables total as of this row. |

## Tier system

| Tier | What change means |
|------|------------------|
| **Stable** | Part of the 1.0 surface. Renaming, retyping or removing it is a breaking change: a deprecation cycle, a CHANGELOG entry and a documented migration are needed. |
| **Tuning** | The name stays, but the default or accepted shape may change in a minor release. Most store and daemon knobs live here. |
| **Experimental** | May be renamed or removed with only a CHANGELOG note. Benefits unproven, or code path behind a cargo feature. |
| **Test** | Not configuration. A test harness's private channel to its spawned child process. Setting one on a real server does nothing useful, so `weft-server` warns at startup. |

For the recovery-point and recovery-time objectives these settings produce, see
[RPO/RTO](RPO-RTO.md). For the complete HTTP API reference, see [reference](reference.md).

## The `weftdb` library and `SegmentStoreOptions`

The `WEFT_SEGMENT_*` variables configure the store `weft-server` opens; the server maps them
to `weftdb::SegmentStoreOptions`. A program embedding `weftdb` gets them only by asking:
`SegmentStore::open` uses `SegmentStoreOptions::default()` (every option unset), and
`SegmentStore::open_with_options(root, SegmentStoreOptions::from_env())` reads them as the
server does. See [reference.md](reference.md#configuration) for the embedded-library story.

## Stable

Thirteen variables. Renaming, retyping or removing one is a breaking change.

| Variable | Summary | Accepts | Default | Read by |
|----------|---------|---------|---------|---------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Export every request's spans to an OpenTelemetry collector over OTLP/gRPC, beside the normal log output. | A collector endpoint, e.g. `http://localhost:4317`; userinfo is allowed (collector basic auth) and is redacted from everything the server prints (SEC-10). | Unset: no exporter and no network dependency; `fmt` logging only. | `weft-server/startup/hooks.rs` |
| `WEFT_ALLOW_ANY_HOST` | Turns off the loopback `Host`/`Origin` guard so a local reverse proxy may forward a different `Host`, or a browser UI a non-loopback `Origin`. | Unset or `0` keeps the guard on; any other value turns it off. | Unset: loopback-only guard (a non-loopback bind is unguarded, which is why it must stay off untrusted networks). | `weft-server/host_guard.rs` |
| `WEFT_ALLOW_PLAINTEXT` | Explicitly opts in to unencrypted traffic on a non-loopback bind, with a mounted keys file: the C8 acknowledgement that plaintext is a deployment choice, never a default (SEC-5). | `1`/`true`/`yes`/`on` (any case) opts in; unset, `0` or anything else does not. | Unset: a non-loopback plaintext bind without TLS or `WEFT_AUTH=disabled` is refused at startup. | `weft-server/tls.rs` |
| `WEFT_AUDIT_FILE` | The fsynced append-only JSONL audit log (SEC-9, decision 8): an attempt and an outcome record per admin or mutating operation, fail-closed on `audit_unavailable`, plus `weft_audit` events from committed ingests, auth denials and daemon supervision. | A file path; the parent directory is created when absent, and a file that cannot be opened or ends in a torn record stops the server from starting. | Unset: `<store root>/audit.jsonl`; with no store configured, `./weft-audit.jsonl`. | `weft-server/audit.rs` |
| `WEFT_AUTH` | The authentication mode (SEC-6). `keys` turns Bearer API-key authentication on and requires a mounted keys file; `disabled` turns auth explicitly off — the gateway deployment's opt-in, which carries any non-loopback bind (C8, decision 7; SEC-8's exposure matrix). | `keys` or `disabled` (any case); any other value stops the server from starting. | Unset: authentication is on when `WEFT_KEYS_FILE` names a mounted keys file, off otherwise. | `weft-server/auth.rs` |
| `WEFT_BACKUP_DIR` | Where control-plane backups are written — by the scheduled daemon and by `POST /api/v1/storage/backup` into one place. | A directory path (absolute, or relative to the process's working directory). | `<store root>/backups`. | `weft-server/backup_routes.rs`, `startup/daemons.rs` |
| `WEFT_DATA_DIR` | The database's data directory: where a `Database`'s store directories live, and where `weft-tui` writes its log. | A directory path. | `~/.weftdb/data`. | `weftdb/types/database/config.rs`, `weft-tui/main.rs` |
| `WEFT_KEYS_FILE` | The API keys file, mounted into the deployment (the Docker image mounts one): a JSON document of key ids, frozen roles and SHA-256 key hashes that SEC-6 enforces (`weft-server keygen` writes its entries). SEC-5's plaintext gate also reads its presence. | A path to a keys file in the SEC-6 format; a file that cannot be parsed stops the server from starting. | Unset: no keys file — authentication is off unless `WEFT_AUTH=keys` (which stops the server from starting). | `weft-server/auth.rs`, `tls.rs` |
| `WEFT_ON_AMBIGUOUS_COMMIT` | What the server does once its store is write-poisoned by an ambiguous COMMIT: keep serving, or exit and let a supervisor restart into the recovery that settles the commit. | `poison` (or unset) keeps serving; `exit` exits with status 70. | Serve; an unknown value stops the server from starting (PAC-5b strict configuration). | `weft-server/store_open.rs` |
| `WEFT_SEGMENT_STORE_ROOT` | Opens the columnar segment store rooted at this directory, which is what makes the `/api/v1/storage/…` endpoints live at all. | A directory path; the Storage v2 layout is created there if absent. | Unset: no store — the stateless interpolate/downsample API only, and `503` on storage reads. | `weft-server/startup/state.rs` |
| `WEFT_SERVER_ADDR` | The address and port `weft-server` binds. | A `host:port` socket address, e.g. `0.0.0.0:9000`. | `127.0.0.1:8080`; an unparseable value stops the server from starting. | `weft-server/startup/config.rs` |
| `WEFT_TLS_CERT_FILE` | The PEM certificate chain the TLS listener presents (SEC-5). Set with `WEFT_TLS_KEY_FILE`; naming one without the other stops the server from starting. | A path to a PEM file holding the certificate chain. | Unset: no TLS — the listener is plaintext and the C8 gate applies. | `weft-server/tls.rs` |
| `WEFT_TLS_KEY_FILE` | The PEM private key matching `WEFT_TLS_CERT_FILE` (SEC-5). Never logged, never served; read once at startup. | A path to a PEM file holding the private key. | Unset: no TLS — the listener is plaintext and the C8 gate applies. | `weft-server/tls.rs` |

## Tuning

Forty-three variables. The name stays forever, but the default or accepted shape may change in
a minor release.

| Variable | Summary | Accepts | Default | Read by |
|----------|---------|---------|---------|---------|
| `WEFT_ADMIN_ADDR` | Also bind a second, probe-only listener here: `/health` and `/ready`, and no other path on the server answers. | A `host:port` socket address, e.g. `127.0.0.1:9090`; an unparseable value stops the server from starting. | Unset: no second listener — the probes stay on `WEFT_SERVER_ADDR`. | `weft-server/startup/config.rs` |
| `WEFT_BACKUP_INTERVAL_SECS` | How often the backup daemon writes a verified `VACUUM INTO` snapshot of the four control-plane databases. | A whole number of seconds; `0` disables the daemon. | Unset or `0`: no scheduled backups. | `weft-server/startup/daemons.rs` |
| `WEFT_BACKUP_KEEP` | How many daemon-generated snapshots to keep: after each backup the oldest beyond this many are removed. A hand-taken `?label=` snapshot is never a prune candidate. | A whole number of snapshots. | Unset: retain every generated snapshot (packaged deployments set 3). | `weft-server/startup/daemons.rs` |
| `WEFT_COMPACT_TARGET_ROWS` | The size-aware compaction pass of the maintenance scheduler: each aspect's segments coalesce toward this many rows per segment instead of folding into one. | A positive whole number of rows. | 65,536 (`DEFAULT_COMPACT_TARGET_ROWS` in `startup/daemons.rs`) — compaction is on by default (ROB-19); a non-numeric value stops the server from starting. | `weft-server/startup/daemons.rs` |
| `WEFT_DISK_RESERVE_BYTES` | The bytes every ingest or seal must leave available on the store's filesystem (ROB-17): a write that would leave less is refused `507 insufficient_storage` before any frame is written. `GET /api/v1/storage/headroom` and the `weft_disk_*` gauges on `/metrics` report the margin. | A non-negative whole number of bytes; `0` turns the guard off. | 268,435,456 (256 MiB, `DEFAULT_DISK_RESERVE_BYTES` in weftdb); a malformed value stops the server from starting. | `weftdb/types/segment_store.rs` (`SegmentStoreOptions::from_env`), read in store_open.rs |
| `WEFT_EXTRACTED_BATCH_RETENTION_SECS` | How long the queue remembers that a pattern extraction already consumed a batch, which is how long a re-ingested batch is recognised as a duplicate. | A positive whole number of seconds. | 48 h (172,800 s); anything else warns and keeps the default. | `weftdb/types/database/inputs.rs` |
| `WEFT_HEADER_READ_TIMEOUT_SECS` | How long a peer may take to deliver a complete request header block before the connection is closed (SEC-4's header deadline, pre-auth). | A positive whole number of seconds. | 30 s (`DEFAULT_HEADER_READ_TIMEOUT` in `serve.rs`). | `weft-server/serve.rs` |
| `WEFT_INGEST_BODY_TIMEOUT_SECS` | The total deadline one ingest body gets to arrive within (ROB-18): the two-phase `IngestBody` extractor reads the whole body against this one clock, not a per-chunk idle bound, and a body that misses it is refused `408 body_timeout` — a slow-drip sender cannot hold its phase-1 disk reservation open indefinitely. | A positive whole number of seconds. | 300 s (`DEFAULT_INGEST_BODY_TIMEOUT_SECS` in `ingest.rs`). | `weft-server/ingest.rs` |
| `WEFT_INGEST_MAX_INFLIGHT_BYTES` | The ingest routes' in-flight byte budget (D-S14): the registry's gate charges each ingest request's `Content-Length` against it, and a request that would push the total past the budget is refused `503 overloaded` with `Retry-After` instead of queueing. | A positive whole number of bytes. | 256 MiB — one maximum-size body in flight. | `weft-server/ingest.rs` |
| `WEFT_KEEP_ALIVE_TIMEOUT_SECS` | How long a connection may make no read progress before it is closed: the keep-alive idle bound between requests (SEC-4, pre-auth). | A positive whole number of seconds. | 30 s (`DEFAULT_KEEP_ALIVE_TIMEOUT` in `serve.rs`). | `weft-server/serve.rs` |
| `WEFT_LEGACY_DEFAULT_STORAGE` | Makes every newly created legacy aspect seal-backed (D-S19): its measurements are sealed into the aspect's Storage-v2 segment store instead of its rows-mode `measurements.db`. | Exactly `segments` for seal-backed; anything else is rows mode. | Unset: rows mode — measurements live in the per-aspect `measurements.db`, as before D-S19. | `weftdb/types/database/inputs.rs` |
| `WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS` | How long a tripped per-aspect maintenance circuit breaker (ROB-19) leaves its aspect out of store-wide maintenance sweeps before one trial pass is allowed. | A whole number of seconds. | 300 s (`DEFAULT_MAINTENANCE_BREAKER_COOLDOWN` in `weftdb`). | `weftdb/types/segment_store.rs` |
| `WEFT_MAINTENANCE_BREAKER_FAILURES` | How many consecutive failed store-wide maintenance sweep passes trip an aspect's circuit breaker (ROB-19). `0` disables the breaker. | A whole number of failed passes. | 3 (`DEFAULT_MAINTENANCE_BREAKER_FAILURES` in `weftdb`). | `weftdb/types/segment_store.rs` |
| `WEFT_MAX_AT_MULTI_INSTANTS` | The most instants one `/api/v1/storage/{aspect}/at-multi` request may name; a request naming more is refused `422 result_too_large` with the limit (ROB-12). | A positive whole number of instants. | 10,000 (`MAX_AT_MULTI_INSTANTS` in `read_limits.rs`); anything else stops the server from starting. | `weft-server/read_limits.rs` |
| `WEFT_MAX_CONNECTIONS` | The most connections the serve path holds at once; a connection beyond the cap waits in the listen backlog, unread, until one in service closes (SEC-4, pre-auth). | A positive whole number of connections. | Half the process's `RLIMIT_NOFILE` soft limit, clamped to 16..=65,536 (1,024 when the limit cannot be read). | `weft-server/serve.rs` |
| `WEFT_MAX_DOWNSAMPLE_BUCKETS` | The most buckets one `/api/v1/storage/{aspect}/downsample*` response may hold (ROB-12). | A positive whole number of buckets. | 1,000,000 (`MAX_DOWNSAMPLE_BUCKETS` in `read_limits.rs`); anything else stops the server from starting. | `weft-server/read_limits.rs` |
| `WEFT_MAX_INGEST_BYTES` | The one body cap for the four ingest routes (plan C11, D-S14): a body over it is refused `413 payload_too_large` with the limit. | A positive whole number of bytes. | 256 MiB (`DEFAULT_MAX_INGEST_BYTES` in `ingest.rs`). | `weft-server/ingest.rs` |
| `WEFT_MAX_INGEST_DECODED_BYTES` | The ceiling on what a body becomes once decoded (ROB-7): the four ingest routes size the timestamps and value columns their body turns into and refuse `413 payload_too_large` over it. | A positive whole number of bytes. | 1 GiB (`DEFAULT_MAX_INGEST_DECODED_BYTES` in `ingest.rs`). | `weft-server/ingest.rs` |
| `WEFT_MAX_INGEST_ROWS` | The row ceiling for one ingest request (ROB-7): a body that decodes to more rows than this is refused `413 too_many_rows` (before any segment is written). | A positive whole number of rows. | 10,000,000 rows (`DEFAULT_MAX_INGEST_ROWS` in `ingest.rs`). | `weft-server/ingest.rs` |
| `WEFT_MAX_INTERPOLATE_POINTS` | The cap on how many output points one `/api/v1/interpolate*` request may produce. | A positive whole number of points. | 10,000,000 (`MAX_INTERPOLATE_OUTPUT_POINTS`); anything else stops the server from starting. | `weft-server/interpolate.rs` |
| `WEFT_MAX_PAGE_ROWS` | The ceiling on a page of the paginated `/points` and `/value-points` reads (ROB-12). | A positive whole number of rows. | 100,000 (`MAX_PAGE_ROWS` in `read_limits.rs`); anything else stops the server from starting. | `weft-server/read_limits.rs` |
| `WEFT_MAX_READ_ROWS` | The most rows one stored read or export may return — the whole window (ROB-12). | A positive whole number of rows. | 5,000,000 (`MAX_READ_ROWS` in `read_limits.rs`); anything else stops the server from starting. | `weft-server/read_limits.rs` |
| `WEFT_MAX_SERIES_PER_ASPECT` | The most tagged series one aspect may register (TAG-7, TAG-11). | A positive whole number of series. | 100,000 (`DEFAULT_MAX_SERIES_PER_ASPECT` in `weftdb`); a malformed value stops the server from starting. | `weftdb/types/segment_store.rs` |
| `WEFT_QUERY_MAX_INFLIGHT` | The query admission class's in-flight cap (ROB-10, Admission I): at most this many read and stateless-compute requests run at once. | A positive whole number of requests. | 128. | `weft-server/startup/config.rs` |
| `WEFT_READ_TIMEOUT_SECS` | How long the server will keep waiting for one stored read before it answers `504 query_timeout` (ROB-12). | A positive whole number of seconds. | 30 (`DEFAULT_READ_TIMEOUT_SECS` in `read_limits.rs`). | `weft-server/read_limits.rs` |
| `WEFT_RECONCILE_HOT_COLD` | Reconcile sweep mode: sweep every aspect's cold segments each tick and defer only the hot tail, instead of the all-or-nothing threshold sweep. | `1`/`true`/`yes`/`on` (case-insensitive). | Unset or falsey: threshold sweep. | `weft-server/startup/daemons.rs` |
| `WEFT_RECONCILE_INTERVAL_SECS` | How often the maintenance scheduler (the reconcile daemon) sweeps for out-of-order segments (ROB-19). | A whole number of seconds; `0` disables the scheduler. | 60 s (`DEFAULT_MAINTENANCE_INTERVAL_SECS` in `startup/daemons.rs`) — the scheduler is on by default. | `weft-server/startup/daemons.rs` |
| `WEFT_RECONCILE_MAX_SPLITS` | Adds a squash pass to the reconcile daemon: any aspect holding more than this many segments is folded into one. | A positive whole number of segments. | Unset: no squash pass. | `weft-server/startup/daemons.rs` |
| `WEFT_RECONCILE_OVERLAPS` | Adds the cross-segment overlap merge to the reconcile daemon. | `1`/`true`/`yes`/`on` (case-insensitive). | Unset or falsey: no overlap merge. | `weft-server/startup/daemons.rs` |
| `WEFT_RECONCILE_SPLIT_MIN_BYTES` | The floor under which the overlap merge rewrites a component outright instead of splitting its cold prefix off. | A whole number of bytes. | 50 MiB. | `weft-server/startup/daemons.rs` |
| `WEFT_RECONCILE_THRESHOLD` | How many out-of-order segments an aspect must hold before the threshold sweep reconciles it. | A non-negative whole number of segments. | 1 — any aspect with at least one out-of-order segment; a non-numeric value stops the server from starting. | `weft-server/startup/daemons.rs` |
| `WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD` | The ceiling on the timestamp-codec override a checkpointed frame may pay, as blocked bytes over best-codec bytes. | A finite number at or above 1.0 (`1.0` = only when free). | 1.25 (`DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD`). | `weftdb/types/segment_store.rs` |
| `WEFT_SEGMENT_CHECKPOINT_MIN_ROWS` | The segment row floor below which no checkpoint index is written. | A positive whole number of rows. | 8,192 (`DEFAULT_CHECKPOINT_MIN_ROWS`). | `weftdb/types/segment_store.rs` |
| `WEFT_SEGMENT_CHECKPOINT_STRIDE` | Turns on the checkpoint index inside a frame — one entry every this many rows. | A positive whole number of rows (~1024 measured the sweet spot). | Unset or `0`: no checkpoint index; frames byte-for-byte as before. | `weftdb/types/segment_store.rs` |
| `WEFT_SEGMENT_PARTIAL_BASE` | Materializes a partial rollup sidecar per segment at this resolution, so a coarse read of a fine segment skips the fine rows. | One resolution token — `seconds`, `minutes`, `hours`, … | Unset or unparseable: no sidecar. | `weftdb/types/partial_sidecar.rs` |
| `WEFT_SEGMENT_PARTIAL_MIN_ROWS` | The segment row floor below which no partial sidecar is written. | A positive whole number of rows. | 4,096 (`DEFAULT_PARTIAL_SIDECAR_MIN_ROWS`). | `weftdb/types/partial_sidecar.rs` |
| `WEFT_SEGMENT_PARTIAL_TIERS` | The coarser rollup tiers to materialise beside the base, fine to coarse. | A comma-separated list of resolution tokens, e.g. `hours,days`; unparseable entries are dropped. | Unset: base only. | `weftdb/types/partial_sidecar.rs` |
| `WEFT_SHUTDOWN_GRACE_SECS` | The shutdown grace (plan C9): how long the serve path lets live connections finish after the shutdown signal. | A positive whole number of seconds. | 5 s (`SHUTDOWN_DRAIN` in `serve.rs`). | `weft-server/serve.rs` |
| `WEFT_STORE_DATABASE` | Which database inside the store root the storage endpoints read and write. | A database name, e.g. `default` or `v010_legacy`. | `default`, the database `weftdb::SegmentStore::open` uses. | `weft-server/store_open.rs` |
| `WEFT_STORE_SCOPE_PIN` | Whether the server refuses to serve a store root that records a different `(database, subject)` scope than the two `WEFT_STORE_*` variables name (FRE-12b). | `on`/`off` or truthy/falsey. | `on`: a mismatched scope stops the server from starting with status 65 and a message naming the scopes. | `weft-server/store_open.rs` |
| `WEFT_STORE_SUBJECT` | Which subject inside the store's database the storage endpoints read and write. | A subject name. | `default`, the subject `weftdb::SegmentStore::open` uses. | `weft-server/store_open.rs` |
| `WEFT_WORKER_THREADS` | The worker-thread count of the tokio runtime the binary builds (ROB-11). | A positive whole number of threads. | Unset: one worker per available core (the runtime's own default). | `weft-server/runtime.rs` |
| `WEFT_WRITE_STALL_TIMEOUT_SECS` | How long a connection's writes may make no progress (a peer that stopped reading) before the connection is closed (SEC-4, pre-auth). | A positive whole number of seconds. | 10 s (`DEFAULT_WRITE_STALL_TIMEOUT` in `serve.rs`). | `weft-server/serve.rs` |

## Experimental

Eight variables. May be renamed or removed with only a CHANGELOG note. Benefits unproven,
or code path behind a cargo feature.

| Variable | Summary | Accepts | Default | Read by |
|----------|---------|---------|---------|---------|
| `WEFT_BENCH_RUNS_DIR` | The directory of `weft-bench` run manifests that `GET /bench/runs` lists and `GET /bench/runs/{id}` serves by the SHA-256 of the manifest's bytes. | A directory path; it is read per request, so a directory that cannot be read answers `500` rather than stopping the server. | Unset: both routes answer `503 unconfigured`. | `weft-server/bench_routes.rs` |
| `WEFT_FLIGHT_ADDR` | Turns on the read-only Arrow Flight listener (gRPC, HTTP/2) beside the HTTP serve path: `DoGet`, `GetFlightInfo`, `GetSchema` and `ListFlights` over the stored range and value-band reads. | A loopback socket address, e.g. `127.0.0.1:8815`; a non-loopback address stops the server from starting (the listener is plaintext). | Unset: no Flight listener. | `weft-server/flight.rs` |
| `WEFT_GPU_ADAPTER` | The one GPU adapter splimes opens (Phase 5, 5.7), from the adapters `splimes::gpu_adapters()` lists. | A whole number (the adapter's index), `api:<api>` (first adapter on that graphics API), or `name:<text>`/any other text (first adapter whose name contains it, ignoring case). | Unset: splimes' own choice. | `weft-server/gpu.rs` |
| `WEFT_GPU_BACKEND` | Which compute backend the interpolation endpoints run on (ROB-16): `auto` lets splimes choose per grid size, `cpu`/`parallel` pin the CPU, `gpu` pins the GPU. | `auto`, `cpu`, `parallel` or `gpu` (case-insensitive). | `auto`; anything else stops the server from starting. | `weft-server/interpolate.rs` |
| `WEFT_GPU_CALIBRATE` | Whether the server times the interpolation backends at startup — GPU, rayon pool, single core — to find where each overtakes the others. | `0`/`false`/`no`/`off` skips; `force` also calibrates a software adapter (llvmpipe, lavapipe, WARP); anything else calibrates a real GPU. | Auto: calibrate when a real GPU is present. | `weft-server/gpu.rs`, `startup/daemons.rs` |
| `WEFT_GPU_DEVICES` | How many GPU devices splimes opens and spreads interpolation over (Phase 5, 5.7). | `one`, `all` (every hardware adapter), or a comma-separated list of `WEFT_GPU_ADAPTER`-style selectors. | Unset: one device. | `weft-server/gpu.rs` |
| `WEFT_SEGMENT_MMAP_READS` | Range reads memory-map their sealed `.weftseg` frames instead of reading each into a heap buffer (roadmap Phase 5). Needs a build with the `mmap-reads` cargo feature. | `on`/`1`/`true`/`yes` or `off`/`0`/`false`/`no`; anything else stops the server from starting. | Unset: off (buffered reads). | `weftdb/types/segment_store.rs` |
| `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` | Writes a sealed segment's value column in the bit-sliced (bit-plane-major) codec when it costs no more than this ratio over the size-selected codec. Needs `bitsliced-codec` cargo feature. | A finite number above 0 (`1.0` = only when free). | Unset: no transposed codec; frames byte-for-byte as before. | `weftdb/types/segment_store.rs` |

## Test

Forty-one variables. Not configuration. A test harness's private channel to the child
process it spawns, or a switch a test suite reads. Setting one on a real server does
nothing useful — `weft-server` warns at startup. They are registered precisely so that
this doc **cannot** carry one by accident; the env file explicitly excludes Test-tier
variables for the same reason.

Listed here for completeness; see `weft-server/src/settings.rs` for the full spec of each.

PAC-6 completed the registry by removing `TEST_DATA_DIR`, the one Test-tier name that
configured a shipped library path (`weftdb::data_dir`): the test suites are handed their
fixture directory through `WEFT_DATA_DIR` (Stable) instead, so no test-only override of
the database root remains.

| Variable |
|----------|
| `SKIP_SLOW_TESTS` |
| `WEFT_BENCH_BACKUP_DIR` |
| `WEFT_BENCH_INTERPOLATE_POINTS` |
| `WEFT_BTC_CSV` |
| `WEFT_DB_NEW_CRASH_CHILD` |
| `WEFT_FAULT` |
| `WEFT_FAULT_TEST_CHILD` |
| `WEFT_LEGACY_CONSUMER_CRASH_CHILD` |
| `WEFT_LEGACY_INGEST_CRASH_CHILD` |
| `WEFT_REQUIRE_GPU` |
| `WEFT_SERVER_BIN` |
| `WEFT_SI_KNOTS` |
| `WEFT_SI_REPS` |
| `WEFT_SI_ROWS` |
| `WEFT_TEST_AMBIGUOUS_CHILD_ROOT` |
| `WEFT_TEST_AUDIT_FULL_CHILD` |
| `WEFT_TEST_AUDIT_REFUSAL_CHILD` |
| `WEFT_TEST_GLOBAL_POISON_CHILD_ROOT` |
| `WEFT_TEST_KEEP_CONFLICT_CHILD_ROOT` |
| `WEFT_TEST_OPEN_CHILD_ROOT` |
| `WEFT_TEST_ORPHAN_CHILD_ROOT` |
| `WEFT_TEST_PANIC_CHILD_MODE` |
| `WEFT_TEST_POISON_EXIT_CHILD_ROOT` |
| `WEFT_TEST_RACE_CHILD_ROOT` |
| `WEFT_TEST_S8_CONFLICT_CHILD_ROOT` |
| `WEFT_TEST_S8_SPLIT_RACE_ROOT` |
| `WEFT_TEST_S9_DROPPED_SWAP_DIR` |
| `WEFT_TEST_S9_SQUASH_RACE_DIR` |
| `WEFT_TEST_S9_SUFFIX_RACE_DIR` |
| `WEFT_TEST_SWAP_MATRIX` |
| `WEFT_TEST_SWAP_MATRIX_DIR` |
| `WEFT_TEST_TAG_SEAL_CRASH_ROOT` |
| `WEFT_TEST_TXN_CHILD_INDEX` |
| `WEFT_UPDATE_GOLDENS` |
| `WEFT_VERBOSE` |
| `WEFT_WIDEN_BENCH_POINTS` |
| `WEFT_XBENCH_CLICKHOUSE` |
| `WEFT_XBENCH_INFLUXDB3` |
| `WEFT_XBENCH_IOTDB` |
| `WEFT_XBENCH_QUESTDB` |
| `WEFT_XBENCH_TIMESCALE` |

## Startup validation

`weft-server` checks its environment before anything else:

- An unrecognised `WEFT_*` name (a typo such as `WEFT_BACKUP_DIRS`) stops it with exit
  status `78` — the configuration error class.
- A registry entry whose value cannot be parsed by its reader also stops the server,
  with exit status `78`, unless the entry's definition says the value is advisory
  (e.g. `WEFT_SEGMENT_PARTIAL_TIERS` drops unparseable items).
- A store it cannot open exits `65` (operator action).
- An unknown write-ambiguous commit mode or an unrecognised `WEFT_STORE_SCOPE_PIN` or
  `WEFT_GPU_*` value stops the server, again with `78`.
- All 104 names in the registry are tested (by `cargo test`) against every crate's `src/`
  to catch any `env::var("WEFT_…")` that is read without being registered, or registered
  without being read.
- The env file `packaging/env/weft-server.env` covers the Tuning and Experimental tiers
  that are safe to set on a deployment node. Test-tier variables are excluded: setting one
  there is an exit-78 refusal, not a silent no-op.

The registry is defined in `weft-server/src/settings.rs` and is the authoritative list.
PAC-5d uses this registry to generate this documentation.