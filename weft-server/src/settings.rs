//! Every environment variable the WeftDB workspace reads, in one registry (1.0 plan
//! PAC-5a).
//!
//! Before this, configuration was a pile of `env::var` calls, one per reader, each with
//! its own idea of what a bad value means. The failure that matters is the quiet one:
//! `WEFT_BACKUP_DIRS=/backups` (one letter too many) is a perfectly good environment
//! variable as far as the process is concerned, and the server backs up nowhere and says
//! nothing. A registry turns that into a start-up message, because the name is the thing
//! an operator types by hand.
//!
//! Two rules, both checked by tests below:
//!
//! 1. **Every variable any crate in the workspace reads is in [`VARS`]** — a new
//!    `env::var("WEFT_…")` anywhere in a crate's `src/` fails CI until it is registered,
//!    and a registry entry nobody reads fails too.
//! 2. **Each entry carries a [`Tier`]**, so "can this change?" has an answer in the
//!    registry rather than in someone's memory: `Stable` is part of the 1.0 promise
//!    (renaming, retyping or removing one is a breaking change), `Tuning` may change its
//!    default or its accepted shape in a minor release, `Experimental` may disappear with
//!    a CHANGELOG note, and `Test` is the plan's allowlist: a test harness's own channel,
//!    never configuration.
//!
//! PAC-5b (strict configuration) and PAC-5c (one read path) build on this registry rather
//! than replacing it: the read sites still read their own variable today, and this module
//! is the source of truth for what the surface *is*. Decision 1(d) is the direction that
//! ends — the library stops reading `WEFT_*` and takes options from here.

/// The scanning helpers below are tests, not startup code: the registry itself reads no
/// file, and the coverage net is only ever run by `cargo test`.
#[cfg(test)]
use std::{
	collections::BTreeSet, path::{Path, PathBuf}
};

/// How much a variable is promised, and therefore what it costs to change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
	/// Part of the 1.0 surface. Renaming, retyping or removing it is a breaking change:
	/// it needs a deprecation cycle, a CHANGELOG entry and a documented migration.
	Stable,
	/// Supported, and the name stays, but its default or its accepted shape may change in
	/// a minor release. Most store and daemon knobs live here, because their values are
	/// deployment-specific and the numbers are still moving.
	Tuning,
	/// May be renamed or removed with only a CHANGELOG note. This is where a knob whose
	/// benefit is unproven, or whose code path is behind a cargo feature, sits until it
	/// earns `Tuning`.
	Experimental,
	/// Not configuration. A test harness's private channel to the child process it
	/// spawns, or a switch a test suite reads. Setting one on a real server does nothing
	/// useful, so [`warnings_for`] says so. This is the plan's "test allowlist".
	Test,
}

impl Tier {
	/// The tier's name as it reads in a message or a documentation table.
	#[must_use]
	pub const fn label(self) -> &'static str {
		match self {
			Self::Stable => "stable",
			Self::Tuning => "tuning",
			Self::Experimental => "experimental",
			Self::Test => "test",
		}
	}
}

/// One environment variable: what it is for, what it accepts, what happens when it is
/// unset, who reads it, and how much it is promised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarSpec {
	/// The variable's exact name, as the operator types it.
	pub name: &'static str,
	/// How much it is promised ([`Tier`]).
	pub tier: Tier,
	/// One sentence on what setting it changes, in the operator's terms.
	pub summary: &'static str,
	/// What a valid value looks like.
	pub accepts: &'static str,
	/// What happens when it is unset, empty, or (where the reader defaults rather than
	/// failing) wrong. `"-"` when there is no fallback worth naming.
	pub default: &'static str,
	/// Where the read lives today, as `crate/path`. PAC-5c moves reads; this field says
	/// what has moved and what has not.
	pub read_by: &'static str,
}

/// Every variable the workspace reads, sorted by name.
///
/// The `every_environment_name_the_workspace_reads_is_registered` test walks every crate's
/// `src/` and fails if a name is read without being listed here, or listed without being
/// read, so this list cannot drift from the code it describes.
pub const VARS: &[VarSpec] = &[
	VarSpec { name: "OTEL_EXPORTER_OTLP_ENDPOINT", tier: Tier::Stable, summary: "Export every request's spans to an OpenTelemetry collector over OTLP/gRPC, beside the normal log output.", accepts: "a collector endpoint, e.g. `http://localhost:4317`", default: "unset: no exporter and no network dependency; `fmt` logging only", read_by: "weft-server/startup/hooks.rs" },
	VarSpec { name: "SKIP_SLOW_TESTS", tier: Tier::Test, summary: "Skips the slow orchestration scenarios. No shipped binary reads it.", accepts: "any value means skip", default: "unset: the slow scenarios run", read_by: "weft-orchestration/lib.rs (test bodies)" },
	VarSpec { name: "TEST_DATA_DIR", tier: Tier::Test, summary: "Points the test suites at their own data directory, overriding `WEFT_DATA_DIR`. PAC-6 removes it once the suites are handed a fixture path instead.", accepts: "a directory path", default: "unset: the suites follow `WEFT_DATA_DIR`, else the default data directory", read_by: "weftdb/types/database/config.rs, weft-orchestration/test_support.rs" },
	VarSpec { name: "WEFT_ALLOW_ANY_HOST", tier: Tier::Stable, summary: "Turns off the loopback `Host`/`Origin` guard so a local reverse proxy may forward a different `Host`, or a browser UI a non-loopback `Origin`.", accepts: "unset or `0` keeps the guard on; any other value turns it off", default: "unset: loopback-only guard (a non-loopback bind is unguarded, which is why it must stay off untrusted networks)", read_by: "weft-server/host_guard.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_ALLOW_PLAINTEXT", tier: Tier::Stable, summary: "Explicitly opts in to unencrypted traffic on a non-loopback bind, with a mounted keys file: the C8 acknowledgement that plaintext is a deployment choice, never a default (SEC-5).", accepts: "`1`/`true`/`yes`/`on` (any case) opts in; unset, `0` or anything else does not", default: "unset: a non-loopback plaintext bind without TLS or `WEFT_AUTH=disabled` is refused at startup", read_by: "weft-server/tls.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_AUTH", tier: Tier::Stable, summary: "The authentication mode (SEC-6). `keys` turns Bearer API-key authentication on and requires a mounted keys file; `disabled` turns auth explicitly off — the gateway deployment's plaintext opt-in (C8, decision 7).", accepts: "`keys` or `disabled` (any case); any other value stops the server from starting", default: "unset: authentication is on when `WEFT_KEYS_FILE` names a mounted keys file, and off when none is mounted", read_by: "weft-server/auth.rs (the name itself is `weft_server::tls::AUTH_ENV`), read in startup/config.rs" },
	VarSpec { name: "WEFT_BACKUP_DIR", tier: Tier::Stable, summary: "Where control-plane backups are written — by the scheduled daemon and by `POST /api/v1/storage/backup` into one place.", accepts: "a directory path (absolute, or relative to the process's working directory)", default: "`<store root>/backups`", read_by: "weft-server/backup_routes.rs, startup/daemons.rs" },
	VarSpec { name: "WEFT_BACKUP_INTERVAL_SECS", tier: Tier::Tuning, summary: "How often the backup daemon writes a verified `VACUUM INTO` snapshot of the four control-plane databases.", accepts: "a whole number of seconds; `0` disables the daemon", default: "unset or `0`: no scheduled backups", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_BACKUP_KEEP", tier: Tier::Tuning, summary: "How many daemon-generated snapshots to keep: after each backup the oldest beyond this many are removed. A hand-taken `?label=` snapshot is never a prune candidate.", accepts: "a whole number of snapshots", default: "unset: retain every generated snapshot (packaged deployments set 3)", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_BENCH_BACKUP_DIR", tier: Tier::Test, summary: "Where the backup-cost benchmark builds and backs up the store it measures, so a bench run can point at scratch space. No shipped binary reads it.", accepts: "a directory path", default: "unset: the benchmark uses its own temporary directory", read_by: "weftdb/benches/backup_cost.rs" },
	VarSpec { name: "WEFT_BTC_CSV", tier: Tier::Test, summary: "The BTC one-minute CSV a compression or decimal benchmark measures against, for the runs that use the real corpus rather than generated data.", accepts: "a path to a CSV file", default: "`database/datasets/btc_1min.csv` beside the workspace", read_by: "weft-physical-type/benches, weft-reduce/benches" },
	VarSpec { name: "WEFT_COMPACT_TARGET_ROWS", tier: Tier::Tuning, summary: "Adds a size-aware compaction pass to the reconcile daemon: each aspect's segments coalesce toward this many rows per segment instead of folding into one.", accepts: "a positive whole number of rows", default: "unset: no compaction pass", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_DATA_DIR", tier: Tier::Stable, summary: "The database's data directory: where a `Database`'s store directories live, and where `weft-tui` writes its log.", accepts: "a directory path", default: "`~/.weftdb/data`", read_by: "weftdb/types/database/config.rs, weft-tui/main.rs" },
	VarSpec { name: "WEFT_DB_NEW_CRASH_CHILD", tier: Tier::Test, summary: "Marks the child process the database-new crash test spawns, so the parent can tell its deliberate crash from a real one.", accepts: "a stage name from the test's own list", default: "unset", read_by: "weftdb/tests/db_tests.rs (test child)" },
	VarSpec { name: "WEFT_EXTRACTED_BATCH_RETENTION_SECS", tier: Tier::Tuning, summary: "How long the queue remembers that a pattern extraction already consumed a batch, which is how long a re-ingested batch is recognised as a duplicate.", accepts: "a positive whole number of seconds", default: "48 h (172800 s); anything else warns and keeps the default", read_by: "weftdb/types/database/inputs.rs" },
	VarSpec { name: "WEFT_FAULT", tier: Tier::Test, summary: "Arms durable fault points so a test can make a commit, an fsync or a swap fail at a chosen step. Not configuration: it exists to make crashes testable.", accepts: "a comma-separated list of `<point>:abort` or `<point>:err`, e.g. `S-frame-synced:abort`", default: "unset: no fault armed (the points are not even compiled without the `fault-injection` feature)", read_by: "weftdb/types/durable/fault.rs" },
	VarSpec { name: "WEFT_FAULT_TEST_CHILD", tier: Tier::Test, summary: "Marks the child process a fault-injection test spawns, so its parent can tell a deliberate abort from a real crash.", accepts: "any value", default: "unset", read_by: "weftdb/types/durable/fault.rs" },
	VarSpec { name: "WEFT_GPU_CALIBRATE", tier: Tier::Experimental, summary: "Whether the server times the interpolation backends at startup — GPU, rayon pool, single core — to find where each overtakes the others.", accepts: "`0`/`false`/`no`/`off` skips; `force` also calibrates a software adapter (llvmpipe, lavapipe, WARP); anything else calibrates a real GPU", default: "auto: calibrate when a real GPU is present", read_by: "weft-server/gpu.rs, read in startup/daemons.rs" },
	VarSpec { name: "WEFT_HEADER_READ_TIMEOUT_SECS", tier: Tier::Tuning, summary: "How long a peer may take to deliver a complete request header block before the connection is closed (SEC-4's header deadline, pre-auth).", accepts: "a positive whole number of seconds", default: "30 s (`DEFAULT_HEADER_READ_TIMEOUT` in serve.rs)", read_by: "weft-server/serve.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_KEEP_ALIVE_TIMEOUT_SECS", tier: Tier::Tuning, summary: "How long a connection may make no read progress before it is closed: the keep-alive idle bound between requests (SEC-4, pre-auth).", accepts: "a positive whole number of seconds", default: "30 s (`DEFAULT_KEEP_ALIVE_TIMEOUT` in serve.rs)", read_by: "weft-server/serve.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_KEYS_FILE", tier: Tier::Stable, summary: "The API keys file, mounted into the deployment (the Docker image mounts one): a JSON document of key ids, frozen roles and SHA-256 key hashes that SEC-6 enforces (`weft-server keygen` writes its entries). SEC-5's plaintext gate also reads its presence.", accepts: "a path to a keys file in the SEC-6 format (see weft-server/auth.rs); a file that cannot be parsed stops the server from starting", default: "unset: no keys file — authentication is off (and `WEFT_AUTH=keys` then refuses to start), and a non-loopback plaintext bind needs TLS or `WEFT_AUTH=disabled`", read_by: "weft-server/auth.rs and tls.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_LEGACY_CONSUMER_CRASH_CHILD", tier: Tier::Test, summary: "Names the stage the legacy-queue consumer crash child dies at, so the crash matrices can arm one point per child.", accepts: "a stage name from the test's own list", default: "unset", read_by: "weft-orchestration/tests/legacy_queue_crash.rs (test child)" },
	VarSpec { name: "WEFT_LEGACY_INGEST_CRASH_CHILD", tier: Tier::Test, summary: "Names the stage the legacy-queue ingest crash child dies at — the other half of the same crash matrix.", accepts: "a stage name from the test's own list", default: "unset", read_by: "weft-orchestration/tests/legacy_queue_crash.rs (test child)" },
	VarSpec { name: "WEFT_MAX_CONNECTIONS", tier: Tier::Tuning, summary: "The most connections the serve path holds at once; a connection beyond the cap waits in the listen backlog, unread, until one in service closes (SEC-4, pre-auth).", accepts: "a positive whole number of connections", default: "half the process's `RLIMIT_NOFILE` soft limit, clamped to 16..=65,536 (1,024 when the limit cannot be read)", read_by: "weft-server/serve.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_MAX_INTERPOLATE_POINTS", tier: Tier::Tuning, summary: "The cap on how many output points one `/api/v1/interpolate*` request may produce.", accepts: "a positive whole number of points", default: "10,000,000 (`MAX_INTERPOLATE_OUTPUT_POINTS`); anything else stops the server from starting", read_by: "weft-server/interpolate.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_ON_AMBIGUOUS_COMMIT", tier: Tier::Stable, summary: "What the server does once its store is write-poisoned by an ambiguous COMMIT: keep serving, or exit and let a supervisor restart into the recovery that settles the commit.", accepts: "`poison` (or unset) keeps serving; `exit` exits with status 70", default: "serve; an unknown value stops the server from starting (PAC-5b strict configuration)", read_by: "weft-server/store_open.rs (the name itself is `weftdb::AMBIGUOUS_COMMIT_ENV`)" },
	VarSpec { name: "WEFT_RECONCILE_HOT_COLD", tier: Tier::Tuning, summary: "Reconcile sweep mode: sweep every aspect's cold segments each tick and defer only the hot tail, instead of the all-or-nothing threshold sweep.", accepts: "`1`/`true`/`yes`/`on` (case-insensitive)", default: "unset or falsey: threshold sweep", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_RECONCILE_INTERVAL_SECS", tier: Tier::Tuning, summary: "How often the reconcile daemon sweeps for out-of-order segments — and, with the other `WEFT_RECONCILE_*` settings, for overlaps, squash and compaction.", accepts: "a whole number of seconds; `0` disables the daemon", default: "unset or `0`: no reconcile daemon; a non-numeric value stops the server from starting", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_RECONCILE_MAX_SPLITS", tier: Tier::Tuning, summary: "Adds a squash pass to the reconcile daemon: any aspect holding more than this many segments is folded into one, bounding split-path fragmentation.", accepts: "a positive whole number of segments", default: "unset: no squash pass", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_RECONCILE_OVERLAPS", tier: Tier::Tuning, summary: "Adds the cross-segment overlap merge to the reconcile daemon, for late data that re-entered an already-covered window. Independent of the intra-segment sweep.", accepts: "`1`/`true`/`yes`/`on` (case-insensitive)", default: "unset or falsey: no overlap merge", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_RECONCILE_SPLIT_MIN_BYTES", tier: Tier::Tuning, summary: "The floor under which the overlap merge rewrites a component outright instead of splitting its cold prefix off.", accepts: "a whole number of bytes", default: "50 MiB", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_RECONCILE_THRESHOLD", tier: Tier::Tuning, summary: "How many out-of-order segments an aspect must hold before the threshold sweep reconciles it.", accepts: "a non-negative whole number of segments", default: "1 — any aspect with at least one out-of-order segment; a non-numeric value stops the server from starting", read_by: "weft-server/startup/daemons.rs" },
	VarSpec { name: "WEFT_REQUIRE_GPU", tier: Tier::Test, summary: "Makes a Weft-Bench run fail instead of skipping when no usable GPU is present.", accepts: "any value", default: "unset: a missing GPU skips the GPU cases", read_by: "weft-bench/gpu_memory.rs" },
	VarSpec { name: "WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD", tier: Tier::Tuning, summary: "The ceiling on the timestamp-codec override a checkpointed frame may pay, as blocked bytes over best-codec bytes — the gate that stops a column whose shape does not benefit from checkpoints being bloated for a lookup win.", accepts: "a finite number at or above 1.0 (`1.0` = only when free)", default: "1.25 (`DEFAULT_CHECKPOINT_MAX_CODEC_OVERHEAD`)", read_by: "weftdb/types/segment_store.rs (`CheckpointPolicy::from_env`)" },
	VarSpec { name: "WEFT_SEGMENT_CHECKPOINT_MIN_ROWS", tier: Tier::Tuning, summary: "The segment row floor below which no checkpoint index is written: a small segment decodes trivially, so indexing it is pure cost.", accepts: "a positive whole number of rows", default: "8,192 (`DEFAULT_CHECKPOINT_MIN_ROWS`)", read_by: "weftdb/types/segment_store.rs (`CheckpointPolicy::from_env`)" },
	VarSpec { name: "WEFT_SEGMENT_CHECKPOINT_STRIDE", tier: Tier::Tuning, summary: "Turns on the checkpoint index inside a frame — one entry every this many rows — trading about 0.4% more bytes for a bounded timestamp scan.", accepts: "a positive whole number of rows (~1024 measured the sweet spot)", default: "unset or `0`: no checkpoint index; the frames WeftDB has always written", read_by: "weftdb/types/segment_store.rs (`CheckpointPolicy::from_env`)" },
	VarSpec { name: "WEFT_SEGMENT_PARTIAL_BASE", tier: Tier::Tuning, summary: "Materializes a partial rollup sidecar per segment at this resolution, so a coarse read of a fine segment skips the fine rows.", accepts: "one resolution token — `seconds`, `minutes`, `hours`, …", default: "unset or unparseable: no sidecar", read_by: "weftdb/types/partial_sidecar.rs (`PartialSidecarPolicy::from_env`)" },
	VarSpec { name: "WEFT_SEGMENT_PARTIAL_MIN_ROWS", tier: Tier::Tuning, summary: "The segment row floor below which no partial sidecar is written.", accepts: "a positive whole number of rows", default: "4,096 (`DEFAULT_PARTIAL_SIDECAR_MIN_ROWS`)", read_by: "weftdb/types/partial_sidecar.rs (`PartialSidecarPolicy::from_env`)" },
	VarSpec { name: "WEFT_SEGMENT_PARTIAL_TIERS", tier: Tier::Tuning, summary: "The coarser rollup tiers to materialize beside the base, fine to coarse, so an unbounded read pays one level each instead of re-materializing.", accepts: "a comma-separated list of resolution tokens, e.g. `hours,days`; unparseable entries are dropped", default: "unset: base only", read_by: "weftdb/types/partial_sidecar.rs (`PartialSidecarPolicy::from_env`)" },
	VarSpec { name: "WEFT_SEGMENT_STORE_ROOT", tier: Tier::Stable, summary: "Opens the columnar segment store rooted at this directory, which is what makes the `/api/v1/storage/...` endpoints live at all.", accepts: "a directory path; the Storage v2 layout is created there if absent", default: "unset: no store — the stateless interpolate/downsample API only, and `503` on storage reads", read_by: "weft-server/startup/state.rs" },
	VarSpec { name: "WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD", tier: Tier::Experimental, summary: "Writes a sealed segment's value column in the bit-sliced (bit-plane-major) codec when it costs no more than this ratio over the size-selected codec. The decode-latency win that motivated it did not survive the word-wise linear unpack, and the codec sits behind the `bitsliced-codec` cargo feature, so this is the clearest candidate for removal.", accepts: "a finite number above 0 (`1.0` = only when free)", default: "unset: the ordinary size-selected codec; a build without the feature warns once and ignores it", read_by: "weftdb/types/segment_store.rs (`TransposedPolicy::from_env`)" },
	VarSpec { name: "WEFT_SERVER_ADDR", tier: Tier::Stable, summary: "The address and port `weft-server` binds.", accepts: "a `host:port` socket address, e.g. `0.0.0.0:9000`", default: "`127.0.0.1:8080`; an unparseable value stops the server from starting", read_by: "weft-server/startup/config.rs" },
	VarSpec { name: "WEFT_STORE_DATABASE", tier: Tier::Tuning, summary: "Which database inside the store root the storage endpoints read and write. A Storage v2 root holds several databases, so this says whose declared aspects and rows a request sees — the container half of the store's scope.", accepts: "a database name, e.g. `default` or `v010_legacy`", default: "`default`, the database `weftdb::SegmentStore::open` uses, so a store written before this variable existed keeps opening the same way", read_by: "weft-server/store_open.rs" },
	VarSpec { name: "WEFT_STORE_SUBJECT", tier: Tier::Tuning, summary: "Which subject inside the store's database the storage endpoints read and write: the namespace the aspect catalog is keyed by, and the one `weft-server import-rows-mode` writes a migrated series into.", accepts: "a subject name", default: "`default`, the subject `weftdb::SegmentStore::open` uses", read_by: "weft-server/store_open.rs" },
	VarSpec { name: "WEFT_TEST_AMBIGUOUS_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child process the ambiguous-COMMIT test spawns: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store.rs (test child)" },
	VarSpec { name: "WEFT_TEST_GLOBAL_POISON_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child process the process-wide-poison test spawns: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store.rs (test child)" },
	VarSpec { name: "WEFT_TEST_OPEN_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child process the open-recovery test spawns: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store.rs (test child)" },
	VarSpec { name: "WEFT_TEST_ORPHAN_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child process that seals a frame and crashes before its commit: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store.rs (test child)" },
	VarSpec { name: "WEFT_TEST_PANIC_CHILD_MODE", tier: Tier::Test, summary: "Names which half of the control-plane panic test the child process is exercising, so its parent can tell a deliberate abort from a real crash. No shipped binary reads it.", accepts: "`inside` panics while the control-plane write scope is held; `outside` panics in an ordinary task", default: "unset: the child returns and the test binary exits 0", read_by: "weft-server/tests/control_plane_panic.rs (test child)" },
	VarSpec { name: "WEFT_TEST_POISON_EXIT_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child `weft-server` the poison-exit test re-invokes: the store root it opens, so the parent can point the child at the poisoned store.", accepts: "a directory path", default: "unset", read_by: "weft-server/tests/poison_exit.rs (test child)" },
	VarSpec { name: "WEFT_TEST_RACE_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child process that reconciles while a squash races it: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store.rs (test child)" },
	VarSpec { name: "WEFT_TEST_S8_CONFLICT_CHILD_ROOT", tier: Tier::Test, summary: "Set only in the child process whose reconcile is merged by a racing squash: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_S8_SPLIT_RACE_ROOT", tier: Tier::Test, summary: "Set only in the child process whose split races a newer segment's commit: the store root it opens.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_S9_DROPPED_SWAP_DIR", tier: Tier::Test, summary: "Set only in the child process that drops a swap and leaves its outputs to the reaper: the directory it works in.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_S9_SQUASH_RACE_DIR", tier: Tier::Test, summary: "Set only in the child process that seals against a racing two-member squash: the directory it works in.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_S9_SUFFIX_RACE_DIR", tier: Tier::Test, summary: "Set only in the child process racing an overlap split's suffix against a later seal: the directory it works in.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_SWAP_MATRIX", tier: Tier::Test, summary: "Names which cell of the swap/crash matrix a child process runs, so each cell arms its faults in its own process.", accepts: "a `<mode>:<op>` spec", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_SWAP_MATRIX_DIR", tier: Tier::Test, summary: "The work directory handed to the swap-matrix child named by `WEFT_TEST_SWAP_MATRIX`.", accepts: "a directory path", default: "unset", read_by: "weftdb/types/segment_store/swap_tests.rs (test child)" },
	VarSpec { name: "WEFT_TEST_TXN_CHILD_INDEX", tier: Tier::Test, summary: "Set only in the child process that dies inside an `IndexTxn`: the index database it opens.", accepts: "a file path", default: "unset", read_by: "weftdb/types/index_txn.rs (test child)" },
	VarSpec { name: "WEFT_TLS_CERT_FILE", tier: Tier::Stable, summary: "The PEM certificate chain the TLS listener presents (SEC-5). Set with `WEFT_TLS_KEY_FILE`; naming one without the other stops the server from starting.", accepts: "a path to a PEM file holding the certificate chain", default: "unset: no TLS — the listener is plaintext and the C8 gate applies", read_by: "weft-server/tls.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_TLS_KEY_FILE", tier: Tier::Stable, summary: "The PEM private key matching `WEFT_TLS_CERT_FILE` (SEC-5). Never logged, never served; read once at startup.", accepts: "a path to a PEM file holding the private key", default: "unset: no TLS — the listener is plaintext and the C8 gate applies", read_by: "weft-server/tls.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_UPDATE_GOLDENS", tier: Tier::Test, summary: "Re-record the committed byte goldens from what this build writes, instead of failing when the bytes differ. No shipped binary reads it.", accepts: "any value means rewrite the fixture file", default: "unset: a golden test compares bytes and fails on a difference", read_by: "weft-physical-type/tests/golden/harness.rs, weftdb/tests/golden_sidecar.rs" },
	VarSpec { name: "WEFT_VERBOSE", tier: Tier::Test, summary: "Not WeftDB's own: it is `splimes`' verbose-logging switch. WeftDB's benchmarks clear it so a dependency's chatter cannot distort the timings they are measuring.", accepts: "any value turns the dependency's verbose output on", default: "unset (and deliberately removed at the top of the benchmarks that care about timings)", read_by: "weftdb/benches (cleared); read by the splimes dependency" },
	VarSpec { name: "WEFT_WRITE_STALL_TIMEOUT_SECS", tier: Tier::Tuning, summary: "How long a connection's writes may make no progress (a peer that stopped reading) before the connection is closed (SEC-4, pre-auth).", accepts: "a positive whole number of seconds", default: "10 s (`DEFAULT_WRITE_STALL_TIMEOUT` in serve.rs)", read_by: "weft-server/serve.rs, read in startup/config.rs" },
	VarSpec { name: "WEFT_XBENCH_CLICKHOUSE", tier: Tier::Test, summary: "Points the cross-engine benchmark's live ClickHouse test at a server, so the ClickHouse arms are checked against WeftDB's reference. No shipped binary reads it.", accepts: "a ClickHouse HTTP endpoint, e.g. `http://127.0.0.1:8123`", default: "unset: the live test returns without checking anything", read_by: "weft-xbench/lib.rs (test body)" },
	VarSpec { name: "WEFT_XBENCH_INFLUXDB3", tier: Tier::Test, summary: "Points the cross-engine benchmark's live InfluxDB 3 test at a server, so the InfluxDB 3 arm is checked against WeftDB's reference. No shipped binary reads it.", accepts: "an InfluxDB 3 HTTP endpoint, e.g. `http://127.0.0.1:8181`", default: "unset: the live test returns without checking anything", read_by: "weft-xbench/lib.rs (test body)" },
	VarSpec { name: "WEFT_XBENCH_QUESTDB", tier: Tier::Test, summary: "Points the cross-engine benchmark's live QuestDB test at a server, so the QuestDB arms are checked against WeftDB's reference. No shipped binary reads it.", accepts: "a QuestDB server's `host:port`, e.g. `127.0.0.1:9000` (QWP and HTTP share it)", default: "unset: the live test returns without checking anything", read_by: "weft-xbench/lib.rs (test body)" },
	VarSpec { name: "WEFT_XBENCH_TIMESCALE", tier: Tier::Test, summary: "Points the cross-engine benchmark's live TimescaleDB test at a server, so the TimescaleDB arms are checked against WeftDB's reference. No shipped binary reads it.", accepts: "a PostgreSQL connection string for a server with the timescaledb extension, e.g. `postgresql://postgres:<password>@127.0.0.1:5432/xbench`", default: "unset: the live test returns without checking anything", read_by: "weft-xbench/lib.rs (test body)" },
];

/// How far a misspelled name may be from a registered one and still be called a typo.
///
/// Two edits: enough for `WEFT_BACKUP_DIRS` (one letter), `WEFT_BUCKUP_DIR` (one letter),
/// `WEFT_RECONCIL_INTERVAL_SECS` (one letter) and `WEFT_ON_AMBIGUOUS_COMMIT` (one letter);
/// far enough that an unrelated `WEFT_` variable from another program is not mistaken for
/// ours, because a wrong suggestion is worse than none.
const MAX_SUGGEST_DISTANCE: usize = 2;

/// The name prefixes the project owns, lowercased because the comparison is
/// case-insensitive: a `weft_server_addr` in a systemd unit is a mistake worth naming, and
/// the environment on Windows is not case-sensitive anyway.
///
/// An unknown name outside them is somebody else's variable — the environment also holds
/// `PATH`, `RUST_LOG` and every other program's settings, and a server that comments on
/// those stops being usable anywhere else.
const OUR_PREFIXES: &[&str] = &["weft_"];

/// Look up one variable by its exact name.
///
/// [`VARS`] is sorted by name, so this binary-searches instead of carrying a map that
/// could disagree with the list.
#[must_use]
pub fn spec(name: &str) -> Option<&'static VarSpec> {
	VARS.binary_search_by(|spec| spec.name.as_bytes().cmp(name.as_bytes())).ok().map(|index| &VARS[index])
}

/// Every registered variable, sorted by name. PAC-5d generates the configuration
/// documentation from this.
/// Every registered variable, in registry order.
///
/// PAC-5d (generated configuration docs) walks this; so does anything that wants the
/// surface rather than one name.
#[must_use = "the registry is the whole configuration surface; walking it is the point"]
pub fn vars() -> impl Iterator<Item = &'static VarSpec> {
	VARS.iter()
}

/// The registered name closest to `name`, when one is close enough to be the spelling the
/// operator meant: case-insensitive, at most `MAX_SUGGEST_DISTANCE` edits apart, the nearest
/// one if several are equally close.
#[must_use]
pub fn suggest(name: &str) -> Option<&'static str> {
	let misspelled = name.to_ascii_lowercase();
	VARS.iter().map(|spec| (edit_distance(&misspelled, &spec.name.to_ascii_lowercase(), MAX_SUGGEST_DISTANCE), spec.name)).filter(|(distance, _)| *distance <= MAX_SUGGEST_DISTANCE).min_by_key(|(distance, _)| *distance).map(|(_, name)| name)
}

/// One line per environment entry the operator should hear about, in the order the
/// environment lists them:
///
/// - a name in WeftDB's namespace that the registry does not know — with the nearest
///   registered spelling when there is one, because a mistyped variable is silently
///   ignored and that is the most expensive kind of configuration bug; the name is matched
///   case-insensitively, so `weft_backup_dir` is reported too;
/// - a `Test`-tier channel set on a real process, which does nothing useful and usually
///   means a harness's environment leaked into a deployment.
///
/// Unset variables, and variables outside the `WEFT_` namespace the project owns, are not
/// commented on.
#[must_use]
pub fn warnings_for(entries: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
	let mut warnings = Vec::new();
	for (name, _) in entries {
		let known = spec(&name);
		if let Some(spec) = known {
			if spec.tier == Tier::Test {
				warnings.push(format!("{} is a test-harness channel (read by {}), not configuration: it should not be set on a real server", name, spec.read_by));
			}
			continue;
		}
		if !OUR_PREFIXES.iter().any(|prefix| name.to_ascii_lowercase().starts_with(prefix)) {
			continue;
		}
		match suggest(&name) {
			Some(suggestion) => warnings.push(format!("{name} is not a WeftDB variable; did you mean {suggestion}? Setting {name} changes nothing")),
			None => warnings.push(format!("{name} is not a WeftDB variable — no setting of it changes anything, and nothing in this build reads it")),
		}
	}
	warnings
}

/// [`warnings_for`] over this process's actual environment.
#[must_use]
pub fn warnings_from_env() -> Vec<String> {
	warnings_for(std::env::vars_os().filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?))))
}

/// Print what the environment got wrong, once, at startup.
///
/// It runs before the store is opened, so a mistyped name is the first thing an operator
/// reads rather than a behaviour they deduce from a log line three screens down. It never
/// fails startup: an unknown variable is a warning, not a decision. The server's startup
/// does not call this any more — PAC-5b made the surface strict ([`strict_errors_from_env`]
/// refuses, exit [`CONFIG_EXIT_CODE`]); this remains for callers that only want the report.
pub fn report() {
	for warning in warnings_from_env() {
		eprintln!("weft-server: {warning}");
	}
}

/// The `EX_CONFIG` exit status (plan C7): invalid configuration or an unsupported
/// deployment, including filesystem refusals.
pub const CONFIG_EXIT_CODE: i32 = 78;

/// The `EX_DATAERR`-class exit status (plan C7): the store needs operator action —
/// a layout or `min_write_layout` newer than this binary, an unreadable
/// `STORE_FORMAT`, a scope mismatch, an incomplete restore, or frames without a
/// control plane.
pub const STORE_EXIT_CODE: i32 = 65;

/// The exit status (plan C7) for a store root already locked by another process.
pub const ROOT_LOCKED_EXIT_CODE: i32 = 75;

/// Variables whose registry `accepts` shape is a non-negative whole number. A value
/// outside that shape was, before PAC-5b, refused by some readers and silently
/// defaulted by others; strict validation refuses it wherever it appears.
const NON_NEGATIVE_INTS: &[&str] = &["WEFT_BACKUP_INTERVAL_SECS", "WEFT_BACKUP_KEEP", "WEFT_RECONCILE_INTERVAL_SECS", "WEFT_RECONCILE_THRESHOLD", "WEFT_RECONCILE_SPLIT_MIN_BYTES", "WEFT_SEGMENT_CHECKPOINT_STRIDE"];

/// Variables whose registry `accepts` shape is a positive whole number.
const POSITIVE_INTS: &[&str] = &["WEFT_COMPACT_TARGET_ROWS", "WEFT_EXTRACTED_BATCH_RETENTION_SECS", "WEFT_HEADER_READ_TIMEOUT_SECS", "WEFT_KEEP_ALIVE_TIMEOUT_SECS", "WEFT_MAX_CONNECTIONS", "WEFT_MAX_INTERPOLATE_POINTS", "WEFT_RECONCILE_MAX_SPLITS", "WEFT_SEGMENT_CHECKPOINT_MIN_ROWS", "WEFT_SEGMENT_PARTIAL_MIN_ROWS", "WEFT_WRITE_STALL_TIMEOUT_SECS"];

/// One line per *value* the operator should hear about, in the order the environment
/// lists them: a registered numeric variable holding a value outside the shape its
/// registry entry accepts, or the one closed enum whose reader used to warn and keep
/// serving (`WEFT_ON_AMBIGUOUS_COMMIT`: `poison` or `exit`). Variables whose readers
/// define open shapes (paths, endpoints, truthy flags where anything else is simply
/// "off") are not judged here — their own startup phase refuses what it cannot use.
#[must_use]
pub fn value_errors_for(entries: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
	let mut errors = Vec::new();
	for (name, value) in entries {
		let trimmed = value.trim();
		if NON_NEGATIVE_INTS.contains(&name.as_str()) {
			if trimmed.parse::<u64>().is_err() {
				errors.push(format!("{name}={value:?} is not a non-negative whole number, which is the only shape it accepts"));
			}
		} else if POSITIVE_INTS.contains(&name.as_str()) {
			if trimmed.parse::<u64>().ok().filter(|n| *n > 0).is_none() {
				errors.push(format!("{name}={value:?} is not a positive whole number, which is the only shape it accepts"));
			}
		} else if name == "WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD" {
			if trimmed.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 1.0).is_none() {
				errors.push(format!("{name}={value:?} is not a finite number at or above 1.0, which is the only shape it accepts"));
			}
		} else if name == "WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD" {
			if trimmed.parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.0).is_none() {
				errors.push(format!("{name}={value:?} is not a finite number above 0, which is the only shape it accepts"));
			}
		} else if name == "WEFT_ON_AMBIGUOUS_COMMIT" {
			if !(trimmed.is_empty() || trimmed.eq_ignore_ascii_case("poison") || trimmed.eq_ignore_ascii_case("exit")) {
				errors.push(format!("{name}={value:?} is neither `poison` nor `exit`, the only values it accepts"));
			}
		}
	}
	errors
}

/// Every reason strict server configuration (PAC-5b) refuses this environment: the
/// name problems [`warnings_for`] reports (an unknown `WEFT_` name, a `Test`-tier
/// channel set on a real server) plus the value problems [`value_errors_for`]
/// reports. An empty result is a configuration the server will start with; anything
/// else is refused before any phase reads a setting, with exit [`CONFIG_EXIT_CODE`].
#[must_use]
pub fn strict_errors_for(entries: Vec<(String, String)>) -> Vec<String> {
	let mut errors = warnings_for(entries.iter().cloned());
	errors.extend(value_errors_for(entries));
	errors
}

/// [`strict_errors_for`] over this process's actual environment.
#[must_use]
pub fn strict_errors_from_env() -> Vec<String> {
	strict_errors_for(std::env::vars_os().filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?))).collect())
}

/// The Levenshtein distance between two names, both already lowercased, stopping as soon
/// as the distance provably exceeds `stop` (every caller only asks "close enough?").
fn edit_distance(a: &str, b: &str, stop: usize) -> usize {
	let (a, b) = (a.as_bytes(), b.as_bytes());
	// Names differ in length by more than the threshold: no number of edits within it
	// turns one into the other, and that is the cheapest test there is.
	if a.len().abs_diff(b.len()) > stop {
		return stop + 1;
	}
	let mut previous: Vec<usize> = (0..=b.len()).collect();
	let mut current: Vec<usize> = vec![0; b.len() + 1];
	for (row, letter_a) in a.iter().enumerate() {
		current[0] = row + 1;
		let mut row_minimum = current[0];
		for (column, letter_b) in b.iter().enumerate() {
			let substitution = previous[column] + usize::from(letter_a != letter_b);
			current[column + 1] = substitution.min(previous[column + 1] + 1).min(current[column] + 1);
			row_minimum = row_minimum.min(current[column + 1]);
		}
		// The best any later row can reach is this row's minimum, so once that is past
		// the threshold the answer is already "too far".
		if row_minimum > stop {
			return stop + 1;
		}
		std::mem::swap(&mut previous, &mut current);
	}
	previous[b.len()]
}

/// The names of the crates the registry must cover, asserted separately so a crate renamed
/// out of the walk cannot quietly shrink the net.
#[cfg(test)]
const COVERAGE_CRATES: &[&str] = &["weftdb", "weft-server", "weft-tui", "weft-bench", "weft-orchestration"];

/// Names of directories never walked: build output, VCS metadata, the vendored v0.1.0
/// sources under `compat/` (byte-exact from the tag, so their `env::var` calls are not
/// ours to register), and prose or scripts, which read no environment.
#[cfg(test)]
const SKIP_DIRS: &[&str] = &["target", ".git", "compat", "docs", "scripts", "fixtures", "node_modules"];

/// Workspace files that quote environment names which must *not* be registered: the
/// misspellings used as fixtures for the warning itself, and another program's variables.
/// They sit outside the net because their literals are the disease, not the symptom.
#[cfg(test)]
const FIXTURE_FILES: &[&str] = &["src/settings.rs", "tests/config_warnings.rs"];

/// Every `WEFT_`/`TEST_`/`SKIP_`/`OTEL_`-prefixed name that appears as a string literal in
/// any workspace crate's source, comment lines excluded.
///
/// This is the registry's coverage net: a literal is how a name reaches `env::var`, so the
/// set of literals is the set of names the code can actually ask for.
#[cfg(test)]
fn env_names_in_workspace() -> BTreeSet<String> {
	let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("weft-server sits in the workspace root").to_path_buf();
	let fixtures: Vec<PathBuf> = FIXTURE_FILES.iter().map(|file| Path::new(env!("CARGO_MANIFEST_DIR")).join(file)).collect();
	let mut found = BTreeSet::new();
	let mut directories = vec![root];
	while let Some(directory) = directories.pop() {
		let Ok(entries) = std::fs::read_dir(&directory) else { continue };
		for entry in entries.flatten() {
			let path = entry.path();
			let name = entry.file_name().to_string_lossy().into_owned();
			if SKIP_DIRS.iter().any(|skip| *skip == name) || name.starts_with('.') || fixtures.contains(&path) {
				continue;
			}
			if path.is_file() {
				if path.extension().is_some_and(|extension| extension == "rs") {
					found.extend(env_names_in_source(&std::fs::read_to_string(&path).unwrap_or_default()));
				}
			} else {
				directories.push(path);
			}
		}
	}
	found
}

/// The environment-name literals on one source file's code lines.
///
/// A name counts when it is a whole string literal (so `"WEFT_BACKUP_DIR is set"` is not
/// one) and starts with a prefix the project or OpenTelemetry owns. Comment lines are
/// skipped: they document names rather than read them, and a design note about a variable
/// that does not exist yet must not force a registry entry.
#[cfg(test)]
fn env_names_in_source(source: &str) -> BTreeSet<String> {
	const PREFIXES: &[&str] = &["WEFT_", "TEST_", "SKIP_", "OTEL_"];
	let mut found = BTreeSet::new();
	for line in source.lines() {
		let code = line.trim_start();
		if code.starts_with("//") || code.starts_with("/*") || code.starts_with('*') {
			continue;
		}
		// Quoted segments alternate: splitting on `"` yields text, literal, text, literal…
		for candidate in code.split('"').skip(1).step_by(2) {
			if candidate.len() >= 6 && candidate.chars().all(|letter| letter.is_ascii_uppercase() || letter == '_' || letter.is_ascii_digit()) && PREFIXES.iter().any(|prefix| candidate.starts_with(prefix)) {
				found.insert(candidate.to_string());
			}
		}
	}
	found
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A registry that is not sorted cannot be binary-searched, and one with a name that
	/// reads differently in two places is not a registry.
	#[test]
	fn the_registry_is_sorted_and_fully_described() {
		let mut previous: Option<&'static VarSpec> = None;
		for spec in VARS {
			if let Some(previous) = previous {
				assert!(previous.name < spec.name, "{} follows {}, so the list is not sorted and spec()'s binary search is wrong", spec.name, previous.name);
			}
			previous = Some(spec);
			assert!(spec.name.len() >= 5 && spec.name.chars().all(|letter| letter.is_ascii_uppercase() || letter == '_' || letter.is_ascii_digit()), "{:?} is not a variable name", spec.name);
			for (field, value) in [("summary", spec.summary), ("accepts", spec.accepts), ("default", spec.default), ("read_by", spec.read_by)] {
				assert!(!value.trim().is_empty(), "{} describes its {field} as empty", spec.name);
			}
		}
		assert_eq!(VARS.len(), 68, "the registry's size is asserted so an entry added or dropped is a deliberate act");
	}

	/// The registry's whole point: it cannot drift from the code. A name read but not
	/// registered means an operator's typo of it is silent; a name registered but not read
	/// means the documentation promises something the build does not do.
	#[test]
	fn every_environment_name_the_workspace_reads_is_registered() {
		let read = env_names_in_workspace();
		let registered: BTreeSet<String> = VARS.iter().map(|spec| spec.name.to_string()).collect();
		let unregistered: Vec<&str> = read.iter().map(String::as_str).filter(|name| spec(name).is_none()).collect();
		let unread: Vec<&str> = registered.iter().map(String::as_str).filter(|name| !read.contains(*name)).collect();
		assert!(unregistered.is_empty(), "{unregistered:?} are read from the environment by some crate but are not in settings::VARS — add a VarSpec (pick a tier, say what it accepts and what happens unset)");
		assert!(unread.is_empty(), "{unread:?} are in settings::VARS but no crate reads them any more — remove the entry, or the documentation promises a variable this build ignores");
	}

	/// A crate renamed or moved out of the walk would quietly shrink the net above, so the
	/// crates it claims to cover are asserted to be there.
	#[test]
	fn the_coverage_walk_sees_every_crate_it_claims() {
		let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("weft-server sits in the workspace root");
		for crate_name in COVERAGE_CRATES {
			assert!(root.join(crate_name).join("src").is_dir(), "{crate_name}/src is not where the registry's coverage walk expects it — update COVERAGE_CRATES rather than let the net shrink");
		}
	}

	/// Tiers are not decoration: `Test` is the plan's allowlist and `Stable` is the part of
	/// the 1.0 promise named in decision 4 ("stable environment variables"). Both sets are
	/// written out here, so moving a name between tiers is a deliberate, reviewable act
	/// rather than a silent edit in a long list.
	#[test]
	fn the_tiers_are_the_explicit_choice_they_claim() {
		let stable: Vec<&str> = VARS.iter().filter(|spec| spec.tier == Tier::Stable).map(|spec| spec.name).collect();
		assert_eq!(stable, ["OTEL_EXPORTER_OTLP_ENDPOINT", "WEFT_ALLOW_ANY_HOST", "WEFT_ALLOW_PLAINTEXT", "WEFT_AUTH", "WEFT_BACKUP_DIR", "WEFT_DATA_DIR", "WEFT_KEYS_FILE", "WEFT_ON_AMBIGUOUS_COMMIT", "WEFT_SEGMENT_STORE_ROOT", "WEFT_SERVER_ADDR", "WEFT_TLS_CERT_FILE", "WEFT_TLS_KEY_FILE"], "the stable surface is what 1.0 promises; adding to it costs a deprecation cycle later");
		let experimental: Vec<&str> = VARS.iter().filter(|spec| spec.tier == Tier::Experimental).map(|spec| spec.name).collect();
		assert_eq!(experimental, ["WEFT_GPU_CALIBRATE", "WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD"], "both are knobs whose value is unproven (GPU calibration's thresholds, and a codec the measurements retired)");
		let harness: BTreeSet<&str> = VARS.iter().filter(|spec| spec.tier == Tier::Test).map(|spec| spec.name).collect();
		let allowlist: BTreeSet<&str> = ["SKIP_SLOW_TESTS", "TEST_DATA_DIR", "WEFT_FAULT", "WEFT_FAULT_TEST_CHILD", "WEFT_REQUIRE_GPU", "WEFT_TEST_AMBIGUOUS_CHILD_ROOT", "WEFT_TEST_GLOBAL_POISON_CHILD_ROOT", "WEFT_TEST_OPEN_CHILD_ROOT", "WEFT_TEST_ORPHAN_CHILD_ROOT", "WEFT_TEST_PANIC_CHILD_MODE", "WEFT_TEST_POISON_EXIT_CHILD_ROOT", "WEFT_TEST_RACE_CHILD_ROOT", "WEFT_TEST_S8_CONFLICT_CHILD_ROOT", "WEFT_TEST_S8_SPLIT_RACE_ROOT", "WEFT_TEST_S9_DROPPED_SWAP_DIR", "WEFT_TEST_S9_SQUASH_RACE_DIR", "WEFT_TEST_S9_SUFFIX_RACE_DIR", "WEFT_TEST_SWAP_MATRIX", "WEFT_TEST_SWAP_MATRIX_DIR", "WEFT_TEST_TXN_CHILD_INDEX", "WEFT_BENCH_BACKUP_DIR", "WEFT_BTC_CSV", "WEFT_DB_NEW_CRASH_CHILD", "WEFT_LEGACY_CONSUMER_CRASH_CHILD", "WEFT_LEGACY_INGEST_CRASH_CHILD", "WEFT_UPDATE_GOLDENS", "WEFT_VERBOSE", "WEFT_XBENCH_CLICKHOUSE", "WEFT_XBENCH_INFLUXDB3", "WEFT_XBENCH_QUESTDB", "WEFT_XBENCH_TIMESCALE"].into_iter().collect();
		assert_eq!(harness, allowlist, "the Test tier is the plan's allowlist: a channel a test harness sets for the child it spawns, and nothing else may sit in it");
		assert_eq!(VARS.iter().filter(|spec| spec.tier == Tier::Tuning).count(), VARS.len() - stable.len() - experimental.len() - allowlist.len(), "every remaining name is Tuning: a knob whose value is deployment-specific and whose number is still moving");
	}

	/// The failure this module exists for: a name one edit away from a real one, which the
	/// process otherwise ignores completely.
	#[test]
	fn a_misspelled_variable_is_named_with_the_nearest_spelling() {
		for (misspelled, intended) in [("WEFT_BACKUP_DIRS", "WEFT_BACKUP_DIR"), ("WEFT_BUCKUP_DIR", "WEFT_BACKUP_DIR"), ("WEFT_BACKUP_INTERVEL_SECS", "WEFT_BACKUP_INTERVAL_SECS"), ("WEFT_RECONCIL_INTERVAL_SECS", "WEFT_RECONCILE_INTERVAL_SECS"), ("WEFT_ON_AMBIGUOUS_COMIT", "WEFT_ON_AMBIGUOUS_COMMIT"), ("WEFT_SERVER_ADDRA", "WEFT_SERVER_ADDR"), ("weft_server_addr", "WEFT_SERVER_ADDR")] {
			assert_eq!(suggest(misspelled), Some(intended), "{misspelled} should be recognised as {intended}");
			let warnings = warnings_for([(misspelled.to_string(), "1".to_string())]);
			assert_eq!(warnings.len(), 1, "{misspelled} is one warning");
			assert!(warnings[0].contains(intended), "{} names the spelling it meant", warnings[0]);
		}
	}

	/// …and the suggestion bar is not so loose that an unrelated `WEFT_` variable from
	/// another program is reported as ours.
	#[test]
	fn an_unrelated_variable_is_not_mistaken_for_a_typo() {
		assert_eq!(suggest("WEFT_LOG_LEVEL"), None, "no registered name is within two edits, so guessing would be worse than saying nothing");
		let warnings = warnings_for([("WEFT_LOG_LEVEL".to_string(), "debug".to_string())]);
		assert_eq!(warnings.len(), 1);
		assert!(!warnings[0].contains("did you mean"), "{} must not guess", warnings[0]);
	}

	/// The environment holds other programs' variables too. Commenting on those would make
	/// the server unusable anywhere else, so only WeftDB's namespace is ever mentioned.
	#[test]
	fn somebody_elses_variable_says_nothing() {
		assert!(warnings_for([("PATH".to_string(), "/usr/bin".to_string()), ("RUST_LOG".to_string(), "info".to_string()), ("TEST_HOST".to_string(), "127.0.0.1".to_string()), ("SKIP_LOCK_CHECK".to_string(), "1".to_string())]).is_empty(), "none of these is ours to comment on");
	}

	/// PAC-5b strictness: the name problems and the value problems are both refusals,
	/// and a correctly shaped value is not one.
	#[test]
	fn strict_errors_cover_bad_names_and_bad_values() {
		assert!(strict_errors_for(vec![("WEFT_BACKUP_KEEP".into(), "3".into()), ("WEFT_ON_AMBIGUOUS_COMMIT".into(), " EXIT ".into()), ("WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD".into(), "1.25".into())]).is_empty());
		for (name, value) in [("WEFT_BACKUP_KEEP", "-1"), ("WEFT_BACKUP_KEEP", "three"), ("WEFT_MAX_INTERPOLATE_POINTS", "0"), ("WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD", "0.5"), ("WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD", "0"), ("WEFT_ON_AMBIGUOUS_COMMIT", "nonsense")] {
			let errors = strict_errors_for(vec![(name.into(), value.into())]);
			assert_eq!(errors.len(), 1, "{name}={value}: {errors:?}");
			assert!(errors[0].starts_with(name), "{errors:?}");
		}
		assert_eq!(strict_errors_for(vec![("WEFT_LOG_LEVEL".into(), "debug".into())]).len(), 1, "an unknown name is a strict error too");
	}

	/// A registered name set correctly is silent — no noise about a configured server — and
	/// a test harness's channel set on a real process is not.
	#[test]
	fn a_configured_environment_is_silent_and_a_harness_channel_is_not() {
		let configured = [("WEFT_SERVER_ADDR", "0.0.0.0:9000"), ("WEFT_BACKUP_KEEP", "3"), ("WEFT_ON_AMBIGUOUS_COMMIT", "exit"), ("WEFT_SEGMENT_STORE_ROOT", "/var/lib/weftdb"), ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4317")];
		assert!(warnings_for(configured.map(|(name, value)| (name.to_string(), value.to_string()))).is_empty(), "every one of these is registered and in force: a server that is configured correctly must print nothing");
		let warnings = warnings_for([("WEFT_FAULT".to_string(), "S-frame-synced:abort".to_string())]);
		assert_eq!(warnings.len(), 1);
		assert!(warnings[0].contains("test-harness channel"), "{} says what WEFT_FAULT is", warnings[0]);
	}

	#[test]
	fn spec_answers_for_every_registered_name_and_nothing_else() {
		for entry in vars() {
			assert_eq!(spec(entry.name).map(|found| found.name), Some(entry.name), "{} looks itself up", entry.name);
		}
		assert!(spec("WEFT_NOT_A_REAL_VARIABLE").is_none());
		assert!(spec("weft_server_addr").is_none(), "the environment is case-sensitive; a lowercase name is not the same variable");
	}

	#[test]
	fn edit_distance_measures_typos_and_stops_early() {
		assert_eq!(edit_distance("weft_backup_dir", "weft_backup_dirs", 2), 1);
		assert_eq!(edit_distance("weft_backup_dir", "weft_buckup_dir", 2), 1);
		assert_eq!(edit_distance("weft_backup_dir", "weft_backup_dir", 2), 0);
		assert_eq!(edit_distance("weft_backup_dir", "weft_server_addr", 2), 3, "a genuinely different name");
		assert_eq!(edit_distance("weft_backup_dir", "otel_exporter_otlp_endpoint", 2), 3, "the early stop reports past-the-threshold, not the exact distance");
	}

	/// The scan itself, so a coverage failure is never ambiguous about which end broke.
	#[test]
	fn the_source_scan_finds_literals_and_ignores_prose() {
		let names = env_names_in_source("let root = std::env::var(\"WEFT_SEGMENT_STORE_ROOT\").ok();\n\t/// documents `WEFT_MENTIONED_IN_PROSE_ONLY`\n\t// \"WEFT_IN_A_COMMENT\"\n\tlet corpus = \"BTCUSD\";\n\tlet built = format!(\"WEFT_CONSTRUCTED_{name}\");\n\tassert!(stderr.contains(\"WEFT_ON_AMBIGUOUS_COMMIT=exit\"), \"it says why\");\n");
		assert_eq!(names.into_iter().collect::<Vec<_>>(), ["WEFT_SEGMENT_STORE_ROOT"], "a literal in code counts; prose, comments, a corpus name, a constructed name and a sentence that merely quotes a name do not");
	}

	/// Smoke: reading the real environment cannot fail the server's startup.
	#[test]
	fn reporting_on_the_real_environment_cannot_panic() {
		let _ = warnings_from_env();
		report();
	}
}
