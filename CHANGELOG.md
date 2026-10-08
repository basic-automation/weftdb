# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor version bumps may contain breaking changes.

## [Unreleased]

### Added

- **Interpolation grids requested over HTTP are capped** at 10,000,000 points per
  request by default (`weft_server::MAX_INTERPOLATE_OUTPUT_POINTS`), on every
  `/api/v1/interpolate*` endpoint; set `WEFT_MAX_INTERPOLATE_POINTS` to change it. The
  variable is read once at startup, and a value that is not a positive integer stops
  `weft-server` from starting. A larger grid is a `400` that names its size and the
  limit, refused before anything is allocated; before, a fine resolution over a long
  range tried to allocate all of it.
- **`weft-server` calibrates the interpolation backends at startup.** Once the listener
  is bound, it runs `splimes::calibrate()` once in the background on a blocking thread:
  that starts the GPU if there is one, times the single-thread, rayon and GPU backends,
  and sets where `Backend::Auto` switches between them. It takes several seconds and
  never delays serving or fails startup; until it finishes, requests interpolate on the
  CPU with splimes' default thresholds. The adapter (or why there is none) and the
  thresholds are logged. A CPU/software adapter (llvmpipe, lavapipe, WARP) is not
  calibrated unless `WEFT_GPU_CALIBRATE=force`; `WEFT_GPU_CALIBRATE=0` skips calibration.
  Without it, splimes 1.0 never uses the GPU.
- **Weft-Bench calibrates like `weft-server`.** An interpolation run (line protocol or
  `--synthetic`) calls `splimes::calibrate()` once before anything is timed, skipping a
  CPU/software adapter as the server does (it has no counterpart to
  `WEFT_GPU_CALIBRATE=force`), so the numbers come from the engine the server runs on
  that machine; `--no-gpu-calibrate` turns it off. The calibration, the GPU and
  `Backend::Auto`'s thresholds are printed and recorded in the report's
  `metadata.engine` (bench schema v16) and its HTML view.
- **Third-party attribution.** A root `NOTICE`, and a `THIRD-PARTY-NOTICES` file shipped in
  the `weft-physical-type` and `weft-reduce` packages, credit the two Apache-2.0 projects
  whose code is adapted here: the Chimp/Chimp128 codecs (from the authors' reference
  implementation) and the DDSketch quantile sketch (from Datadog's sketches-java), with the
  upstream NOTICE text and the license. The Chimp128 docs no longer credit DuckDB as the
  source.
- **Contribution terms.** Every pull request takes one of two routes, the contributor's
  choice: a DCO sign-off (`git commit -s`) on every commit, or the WeftDB Individual
  Contributor License Agreement ([`CLA.md`](CLA.md), version 1, adapted from the Apache
  Software Foundation's ICLA with Justin Icenhour as the recipient), signed once by a pull
  request comment. The `contribution-terms` check passes a pull request when either holds.
  Contributions are licensed `MIT OR Apache-2.0`. See
  [`CONTRIBUTING.md`](CONTRIBUTING.md#contribution-terms).

- **`Inputs::register_dictionary_if_absent`** registers a dictionary as
  `set_dictionary_metadata` does unless a complete registration of that name exists,
  checking inside its own write transaction, and returns whether it registered it. A
  registration that cannot be read counts as existing and is left alone, and a write
  that loses an MVCC conflict is tried once more after a short random delay. The check
  sees registrations committed before its transaction began: one committed while it
  runs, like a second writer registering the same new dictionary at the same moment, is
  not seen, both are written, and reads pick the newest.

### Changed

- **`splimes` moved to its own repository** ([basic-automation/splimes](https://github.com/basic-automation/splimes))
  and is now a crates.io dependency. It is released on its own schedule, and a stable
  WeftDB waits on a stable splimes.
- **splimes 0.1 → 1.0.** WeftDB depends on `splimes = "1"`, resolved to 1.0.0 from
  crates.io. splimes' [migration guide](https://github.com/basic-automation/splimes/blob/main/MIGRATING.md)
  lists the results that change; through WeftDB:
  - large inputs keep their method (0.1 swapped cubic for quadratic from 2,500 input
    points, and cubic and quadratic for linear above 5,000);
  - `Polynomial(1 | 2 | 3, b)` stays a polynomial, so it extends outside the data
    instead of holding flat, and with too few points a polynomial steps down to degree
    `n − 1`. 0.1 stepped down to `Cubic`/`Quadratic`/`Linear` with four input points or
    fewer, and with five or more kept the requested degree and failed;
  - a polynomial degree above 8 is an error instead of being capped at 8;
  - a zero-span range returns its one point: `POST /api/v1/interpolate` with a single
    input point and no explicit range answers `200` with that point (it was a `500`);
  - points that coincide with an input return that input's value exactly, and
    interpolated `BigDecimal`s are the shortest decimal that round-trips;
  - time is exact to the nanosecond at every resolution, duplicate timestamps keep the
    last value, and a leap second is the same instant as the start of the next second;
  - results no longer depend on whether the GPU ran them. With a GPU present, 0.1's
    `auto_interpolate` (behind `analyze_range`, `analyze_point`, compression and every
    `weft-server` interpolation endpoint) ran a call on it when the larger of its input
    count and grid size was 100–999, or either reached 50,000. Its GPU path evaluated a
    polynomial of degree 0 or 4–7 one degree higher than the CPU did (both capped at
    `n − 1` and 8) and, in `f64`, ignored any bounds factor other than 1.0, so those
    calls returned different polynomial values from the CPU's, and on a GPU without
    `f64` support it computed in `f32`. 1.0 computes the same method, in `f64`, on every
    backend;
  - `Backend::Auto` uses the GPU only once the program has started it, and only above
    the calibrated thresholds (see the startup calibration above); without calibration
    every interpolation runs on the CPU.
- **No interpolation runs on an async worker.** splimes 1.0 is synchronous; `weftdb`'s
  `analyze_point`, `analyze_range` and compression, every `weft-server` interpolation
  endpoint, and the Weft-Bench WeftDB adapter run it on tokio's blocking pool
  (`Interpolator::run_async`, splimes' `tokio` feature). 0.1's `async` functions
  computed on the calling worker.
- **Downsample buckets don't move.** splimes 1.0 removed `Resolution::to_base` and the
  `SECONDS_IN_*` constants; the new `weft_reduce::bucket_index` reproduces 0.1's index
  exactly, including its rounding toward zero before 1970, so bucket edges and the keys
  of persisted `.weftpart` sidecars are unchanged. `Resolution::difference` is replaced
  by `weftdb::units_between`, with the same results.
- **Breaking for Rust users: `weftdb::Resolution` and `weftdb::Spline` are
  `#[non_exhaustive]`.** They are re-exports of splimes' types, which 1.0 marks
  non-exhaustive so new variants can arrive in minor releases: an exhaustive `match` on
  either needs a `_` arm. Their removed 0.1 helpers go with them: use
  `Resolution::step()`/`step_nanos()` for `to_step()` and the `SECONDS_IN_*` constants,
  `weftdb::units_between` for `difference`, `weft_reduce::bucket_index` for `to_base`,
  and `Spline::min_points()` for `number_of_points_required()`. `Resolution::round()`
  and `to_step_base()` need no replacement: a step is one unit at every resolution, so
  `to_step_base()` was always `1` and `round()` returned its input unchanged (or, at
  nanosecond resolution outside 1677–2262, an error). `Spline::pre_check()` has no
  direct replacement: `Spline::validate()` checks a polynomial's parameters, and the
  engine checks the range and steps down when there are too few points (or, under
  `Interpolator::exact(true)`, returns `InsufficientPoints`).
- **Stored spline methods are validated when they are loaded.** A dictionary created
  with `AspectStructure::new_dictionary`, or registered by a pipeline through
  `weft_orchestration::load_dictionary` (which stored no constraints before this
  release; see Fixed), persists its step interpolation as the `Spline`'s text
  (`steps_interpolation` in the `dictionary_constraints` table of
  `<aspect>/dictionaries/<dictionary>.db`). splimes 1.0's `Spline::from_str` validates what it parses, so a stored
  `Polynomial` with degree 0, a degree above 8, or a negative or non-finite bounds
  factor, which 0.1 accepted, no longer loads: `get_dictionary_metadata` returns
  `Database error: Invalid interpolation format: invalid polynomial degree 9: must be
  between 1 and 8` (or `… invalid bounds factor -1: must be finite and not negative`),
  and `load_dictionary` logs that error as a warning ("Failed to read dictionary
  metadata; leaving the stored registration as it is") and carries on with the
  pipeline's own constraints, without touching what is stored. Until this release `get_dictionary_metadata` could
  not read any stored dictionary (see Fixed), so this is the first release that reads,
  and so checks, a stored method at all. To fix it, stop WeftDB and rewrite the stored
  value with Turso's shell (`tursodb`, not `sqlite3`: the file is in Turso's MVCC
  journal mode), e.g. `UPDATE dictionary_constraints SET steps_interpolation =
  'Polynomial(degree: 8, bounds_factor: None)' WHERE steps_interpolation =
  'Polynomial(degree: 9, bounds_factor: None)'`; 0.1 capped any degree above 8 at 8.
  Degree 0 and invalid bounds factors have no exact 1.0 equivalent: choose a degree
  from 1 to 8 and a finite, non-negative bounds factor, or `None`. `new_dictionary`
  now checks the method with `Spline::validate()` and refuses one that would not load,
  e.g. `Invalid step interpolation for dictionary 'd': invalid polynomial degree 9: must
  be between 1 and 8`; it stored any method before. `set_dictionary_metadata`, and so
  `load_dictionary` when it registers a dictionary, refuses one the same way.
- `Resolution` names parse case-insensitively (`Hours`, `HOURS`), for example in
  `WEFT_SEGMENT_PARTIAL_BASE`, which ignored anything but lowercase before.
- **`weft-server` provenance comes from splimes.** The `kind` of each interpolated
  point (JSON, CSV, Arrow, Parquet, and the point query) is splimes' `PointKind`, with
  the same `raw` / `interpolated` / `extrapolated` tokens, and `weft_server::PointKind`
  is now a re-export of it. It agrees with the classification `weft-server` did itself
  except at a leap second: an input at `23:59:60.5` and a grid instant at `00:00:00.5`
  the next day are one POSIX instant, so that point is now `raw` (it was `extrapolated`
  or `interpolated`).
- **Invalid polynomial parameters are a `400`.** A `polynomial` spline with degree 0, a
  degree above 8, or a negative `bounds_factor` used to be accepted, and the
  interpolation endpoints usually answered `200` with a result (0.1 capped a degree
  above 8 at 8). Not always: with 5 to `degree` input points a degree above 8 was a
  `500` (0.1's failed step-down, above), and on a machine without a usable GPU, a
  request whose larger of input count and grid size was 100–999 or 50,000 and up ran
  0.1's SIMD path, where degree 0 was a `500` and a bounds factor below −0.5 panicked
  once it extrapolated non-constant data, dropping the connection without a response.
  splimes 1.0 rejects them, and the request is now a `400` naming the problem, e.g.
  `invalid polynomial degree 9: must be between 1 and 8`. (JSON cannot carry a
  non-finite bounds factor; such a body is a `400` from the JSON parser before splimes
  sees it.)
- **Interpolation errors map to HTTP status by kind.** An input or extrapolated value
  beyond `f64`'s range and an oversized grid are `400`s too; any other engine error
  (a GPU failure, a panicked blocking task) stays a `500`, as every engine error was
  before.
- **Turso control plane upgraded 0.6 → 0.8.** ⚠️ This is one-way: once 0.8 writes a
  store, its MVCC log is v3 and an older WeftDB can no longer open it. Back up the
  control plane before upgrading.
- **`.weftpart` sidecars use `postcard` instead of `bincode`** (frame v3). Sidecars
  written by older versions are ignored: the segment is decoded instead, and the
  sidecar is rebuilt on the next seal. No data migration is needed.
- `weft-bench --spline poly:N` rejects a degree outside 1–8 when the arguments are
  parsed, naming the limit. 0.1.0 accepted any degree, and with splimes 1.0 such a run
  would only have failed once the engine rejected it.
- All dependencies updated to their latest major versions, including wgpu 30,
  Arrow/Parquet 60 and OpenTelemetry 0.33.
- Builds on stable Rust (MSRV 1.95); nightly is no longer required.
- **Advisory codecs moved behind the `experimental-codecs` feature** (`weft-physical-type`).
  ⚠️ Breaking for code that calls them: the `floatcodec` module (Gorilla-XOR, Chimp,
  Chimp128, Elf and `best_f64_*`), `ColumnEncoding::{gorilla_f64_bytes, best_f64_bytes,
  best_f64_codec}` and the FIRE forecaster (`fire_*`) now need
  `features = ["experimental-codecs"]`. None of them was ever written to disk, so stored
  segments are unaffected. The feature is off by default, outside the semver promise, and
  pending patent review.
- **The opt-in transposed value codec is now the `bitsliced-codec` feature**
  (`weft-physical-type`, forwarded by `weftdb` and `weft-server`), pending patent review.
  ⚠️ A store that enabled it with `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` must be built with
  `bitsliced-codec` to read those segments: without the feature, reading one fails with the
  new `WeftSegError::CodecNotEnabled` (which names the feature) instead of decoding, and the
  variable is ignored with a warning. For library callers, without the feature
  `FrameOptions::transposed_max_overhead` is accepted but silently ignored
  (`write_segment_with` and `write_paged_segment_with` emit the size-selected codec), and
  `WeftSegError` gains the `CodecNotEnabled` variant, which breaks exhaustive matches on it.
  The `transpose_bitpack_*` primitives, `TRANSPOSE_TILE`,
  `ColumnEncoding::{transposed_value_bytes, transposed_overhead, best_value_codec_transposed}`
  and `weftseg::write_value_column_transposed` need the feature too. The default
  configuration never wrote this codec, so a store that never set the variable (or
  `FrameOptions::transposed_max_overhead`) is unaffected. The docs now call it a bit-sliced
  (bit-plane-major) layout; it is not the FastLanes layout they used to name.
- **WeftDB is dual-licensed under MIT OR Apache-2.0**, at your option, from this release on.
  Every crate declares `license = "MIT OR Apache-2.0"`, and the repository root and every
  crate ship `LICENSE-MIT` and `LICENSE-APACHE` in place of `LICENSE`. The release archives
  carry both files, `NOTICE` and the `THIRD-PARTY-NOTICES` files. Earlier commits remain
  available under MIT, as they were published. The portions adapted from Chimp and
  DDSketch stay under Apache-2.0 whichever option you choose.
- The MIT license text (now `LICENSE-MIT`) reads `Copyright (c) 2025-2026 Justin Icenhour`,
  naming the individual copyright holder for both years of the project.

### Fixed

- **Interpolation responses report the method that ran.** `spline` in the
  `/api/v1/interpolate` and `/api/v1/interpolate/ilp` JSON responses and in the point
  query is documented as the method actually used, but echoed the requested one. With
  fewer distinct timestamps than a method needs, splimes steps down (`Cubic` →
  `Quadratic` → `Linear`, `Polynomial(d, b)` → `Polynomial(n − 1, b)`), so a `cubic`
  request over three points ran a quadratic and still said `Cubic`; it now says
  `Quadratic`. The CSV, Arrow and Parquet outputs carry no method field. The
  `interpolate.engine` trace span keeps the requested method in `spline` and now
  records the one that ran as `effective_spline`.
- **`get_dictionary_metadata` and `list_dictionaries` failed for every stored
  dictionary.** Both selected an `updated_at` column that the `dictionary_metadata`
  table does not have (`Failed to query dictionary metadata: … no such column:
  updated_at`), and `get_dictionary_metadata` then read `steps_count`, an `INTEGER`
  column, as text. They read the table as it is now: the count as an integer (text is
  still accepted), and a dictionary without steps as `None`. The metadata
  `get_dictionary_metadata` caches is keyed by aspect as well as name, so two aspects'
  dictionaries of the same name no longer share it.
- **A pipeline's dictionaries lost their steps and variabilities, and gained a metadata
  row on every load.** `set_dictionary_metadata`, which
  `weft_orchestration::load_dictionary` registers a pipeline's dictionaries with,
  inserted only the `dictionary_metadata` row. The steps and variabilities were never
  stored, `get_dictionary_metadata` (which needs the constraints row) never found the
  dictionary, and so every `load_dictionary` (two per `Pipeline::extract_patterns`)
  registered it again, with another row and a new id. It now writes the whole
  registration and replaces any earlier one of that name, since the tables cannot carry
  a unique constraint; `AspectStructure::new_dictionary` writes the same way, so creating
  a dictionary twice no longer leaves two. `load_dictionary` registers a dictionary only
  when none is registered, under the `Dictionary`'s own id, through the new
  `Inputs::register_dictionary_if_absent`: it checks again inside its write, so a
  registration committed after `load_dictionary` read none and before that write began
  (an explicit `set_dictionary_metadata` with tuned constraints, say) is kept instead of
  replaced with the pipeline's, and a write that loses an MVCC conflict to another load
  healing the same rows is tried once more. A dictionary name that cannot name a file is an
  error from `load_dictionary`. `get_dictionary_metadata`
  now answers `Ok(None)` for a dictionary with no database yet (it was an error,
  `Dictionary '…' does not exist for aspect '…'`), and a registration it cannot read
  is left alone instead of registered again. Of several rows for one name,
  `get_dictionary_metadata` reads the newest that has constraints (it read whichever
  came first), so a dictionary an earlier release registered from a pipeline reads as
  unregistered and is registered again, with its constraints, on its next load, which
  also deletes its extra rows.
- **`get_dictionary_metadata` returned no variabilities.** It always answered `None`,
  although `new_dictionary` stores them. It now reads them back in the order they were
  stored; an empty list stores nothing, and so reads back as `None`.
- **`list_dictionaries` read only a dictionary named "default".** It opened the
  aspect's dictionary named "default", so it failed for an aspect without one
  (`Dictionary 'default' does not exist for aspect '…'`) and otherwise listed only that
  dictionary. Each dictionary is its own `<aspect>/dictionaries/<name>.db`; it now lists
  all of them, by name, each with its steps and variabilities (it returned empty
  constraints), and the list is no longer cached, so it does not go stale when a
  dictionary is added. Only regular files are listed: a symlink, which it followed and
  so opened (setting the target's journal mode and creating tables in it), a directory
  or another entry named `*.db` is skipped. Every regular `*.db` file there is still
  opened as a dictionary, so keep backups out of that directory or under another
  extension. A dictionary whose stored registration does not parse, such as one with a
  stored step method splimes rejects, is logged as a warning and left out instead of
  failing the whole listing; a read that fails (I/O, a query, an MVCC conflict) still
  fails it, so a dictionary is never left out only because it could not be read.
- **A dictionary file without its tables could not be registered.** A dictionary's file
  can exist before its tables, for example when `insert_pattern_into_dictionary` opened
  it first, which creates no tables. Once the file was open in the process nothing created
  them, so `get_dictionary_metadata` failed with `no such table: dictionary_metadata`,
  `load_dictionary` only warned and never registered the dictionary, and
  `list_dictionaries` failed for the whole aspect. Such a file holds no registration, so
  it now reads as unregistered (`Ok(None)`), and registering the dictionary creates the
  tables.
- **A metadata read could cache a registration that had just been replaced.** A
  `get_dictionary_metadata` whose snapshot predated a `set_dictionary_metadata` commit
  could store the old registration after the write had invalidated it, and the old one
  was then served for up to the ten-minute cache lifetime. A read now caches what it read
  only if no write on the same `Database` invalidated the entry since it began
  (`DatabaseCache::generation`, `invalidate_generation` and `store_if_generation`). A
  registration written through `AspectStructure::new_dictionary`, another `Database`
  handle or another process still does not invalidate the cache, and can be served
  stale until the entry expires.
- **Commit failures were silently ignored.** A control-plane write that lost an MVCC
  conflict was reported as success. Commit errors now reach the caller, and the
  transaction is rolled back.
- **A write that lost an MVCC conflict reported a failed rollback instead.** Turso rolls
  a transaction back itself when one of its statements loses a write-write conflict, so
  the `ROLLBACK` after the failed statement fails (`cannot rollback - no transaction is
  active`), and control-plane writes (registering or creating a dictionary, inserting a
  pattern, removing an event, …) returned that error, `Rollback failed: …`, in place of
  the conflict. They now return the statement's error and log the failed rollback as a
  warning. `weftdb::error::is_transient_mvcc_error` also matches Turso's `Busy`,
  `BusySnapshot` and write-write conflict errors anywhere in an error's chain (it
  matched only WeftDB's own `TransientMvccError`), so it recognises the conflict as
  retryable where the returned error keeps Turso's error in its chain: from registering
  or creating a dictionary (`set_dictionary_metadata`, `register_dictionary_if_absent`,
  `AspectStructure::new_dictionary`) and from a failed commit. The other control-plane
  writes still return the statement's error as text only, which it does not recognise.
- **Quadratic GPU interpolation failed on GPUs without f64 support** (Apple Silicon,
  most integrated GPUs, Windows WARP). The f32 fallback shader did not parse, so wgpu
  panicked.

### Security

- **Aspect names can no longer point outside the segment store.** An aspect's name is
  part of the file names of its `.weftseg` frames and `.weftpart` sidecars, and it was
  not checked, so a client of `weft-server` could declare a name that made sealing,
  reconciling or compacting create, overwrite or delete those files outside the store's
  `segments/` directory. Names are now validated wherever they are declared or turned
  into a path (`weftdb::aspect_name::validate`): at most 160 bytes, no `/` or `\`, no
  control characters, no leading `.`, no trailing `.` or space, and not a Windows device
  name (`CON`, `NUL`, `COM1`, `CONIN$`, …, also with an extension or a `:` suffix).
  Bidirectional controls, line and paragraph separators and invisible formatting
  characters are refused as well, so a name cannot display as a different one in
  listings and logs (the zero-width joiner and non-joiner, which some scripts need, are
  still accepted). On
  Windows, `<`, `>`, `:`, `"`, `|`, `?` and `*` are refused too, since such a name could
  never be sealed there. `POST /api/v1/storage/aspects` answers `400` for an invalid
  name, and every frame path is also checked to be a direct child of `segments/`. An
  aspect already declared under such a name was never safe to use: sealing, reading or
  maintaining it now fails with a typed `weftdb::InvalidAspectName` error (a `400` from
  the ingest, read, reconcile, squash and compact endpoints), and the store-wide
  maintenance sweeps list it in `failed` and count it in
  `weft_reconcile_failed_passes_total` while still maintaining every other aspect.
  Listing it and reading its schema or stats, which touch no files, still work. Names
  that differ only in letter case or Unicode normalisation are still accepted and can
  share frame files on a case-insensitive or normalising filesystem; that is a collision
  inside `segments/`, not a way out of it, and the planned encoded frame names remove it.
- **Dictionary names can no longer point outside the aspect's `dictionaries/`
  directory.** Each dictionary is its own `<aspect>/dictionaries/<name>.db`, and the name
  was not checked, so `../x` reached `<aspect>/x.db` and an absolute name such as `/tmp/x`
  replaced the whole path; `get_dictionary_metadata` then created that file and its
  tables, and `new_dictionary` and `set_dictionary_metadata` wrote to it. Dictionary
  names now follow the aspect-name rules (`weftdb::dictionary_name::validate`), checked by
  the dictionary path builders, so every dictionary operation (`new_dictionary`,
  `Aspect::dictionary`, `set_dictionary_metadata`, `insert_pattern_into_dictionary`,
  `get_dictionary_metadata`, `get_dictionary_db`, `get_dictionary_patterns`) refuses
  such a name with a typed `weftdb::InvalidDictionaryName` error before any file is
  touched. `Config::aspect_dictionaries_db_path` and `Config::dictionary_path` return
  `Result<String>` (they returned the path), and `list_dictionaries` skips a `.db` file
  whose name is not a valid dictionary name, with a warning. No `weft-server` endpoint
  takes a dictionary name.

  **Breaking for existing data:** a dictionary an earlier release created under a name
  these rules now refuse (a leading `.`, a trailing `.` or space, a Windows device name
  on any OS such as `aux`, `con`, `com1` or `nul.x`, a control, bidirectional or
  invisible formatting character, or more than 160 bytes) can no longer be reached.
  Every operation on it returns `InvalidDictionaryName`, `load_dictionary` fails the
  pipeline run that uses it, and `list_dictionaries` leaves it out with a warning.
  Nothing is deleted. To recover one, stop everything that uses the database, rename
  `<aspect>/dictionaries/<name>.db`, and the `<name>.db-log` and `<name>.db-wal` files
  beside it if present, to a valid name, then set that name in the `name` column of the
  dictionary's `dictionary_metadata` rows, in the `dictionary_name` column of the
  `pipeline_dictionaries` rows in the aspect's `pipeline.db`, and in the pipeline
  configuration (`PipelineRunConfig`, `PipelineBuilder`) that names it.
- **API callers can no longer make backup retention delete the daemon's snapshots.**
  `WEFT_BACKUP_KEEP` retention keeps the newest `backup-<digits>` directories by their
  embedded timestamp, and `POST /api/v1/storage/backup` created directories in that same
  form, both for a `?label=` in it and for every unlabelled backup, so a caller could fill
  the retained set and get the daemon's genuine snapshots pruned. That form is now
  reserved for the backup daemon: a `?label=` in it is a `400`, and an unlabelled backup
  is now named `manual-<unix_millis>`. A label also may no longer start or end with `.`
  (Windows drops a trailing dot from a directory name, so such a label could still land
  on a reserved name there); the restore drill's `?label=` follows the same rule.
  Snapshots taken through the endpoint are therefore never counted or pruned by
  retention; this is a behaviour change for unlabelled backups, which used to be pruned,
  so remove them by hand when no longer needed. Retention also ignores a
  generated-looking directory whose stamp is more than 24 hours past the current clock or
  past the directory's own modification time, which no daemon snapshot ever is: it is
  not counted and not removed, and each listing logs a warning naming it. A directory
  planted through the API before this release keeps its planting time as its
  modification time, so a far-future stamp stays ignored after the clock reaches it, as
  long as the directory is not modified and its filesystem reports modification times.
  One stamped less than 24 hours past its planting looks like a snapshot from a skewed
  clock and is counted, but no longer outranks new snapshots a day after it was
  planted. Remove such directories by hand. Anyone who can write to the backup directory
  directly can still affect retention; this closes the API path.
- **Web pages can no longer drive a loopback-bound `weft-server`.** With no
  authentication, the default `127.0.0.1` bind was the only protection, but a page open
  in a browser on the same machine could still send requests that need no CORS
  preflight (enough to trigger maintenance, backups and ingest), or reach the API
  through DNS rebinding. While bound to a loopback address (including the IPv4-mapped
  `[::ffff:127.0.0.1]`), the server now answers only requests addressed to `localhost`,
  `127.0.0.0/8` or `[::1]` (`421 Misdirected Request` otherwise, `/health` and `/ready`
  included) and refuses a state-changing request whose `Origin` is not a loopback origin
  (`403`). Clients that send no `Origin`, such as `curl`, are unaffected. Pages served by
  another local web server (any loopback origin, on any port) are still trusted until
  authentication lands. Health probes must send a loopback `Host` such as `localhost` or
  `127.0.0.1`, as every HTTP/1.1 client does; an HTTP/1.0 probe that sends no `Host` now
  gets `421`, so configure it to send one or set `WEFT_ALLOW_ANY_HOST=1`. Behind a local
  reverse proxy that forwards a different `Host`, or the non-loopback `Origin` of a
  browser UI it fronts, set `WEFT_ALLOW_ANY_HOST=1`. Non-loopback binds are unchanged
  and remain unauthenticated.
- Cleared the `crossbeam-epoch` (RUSTSEC-2026-0204) and `h2` (RUSTSEC-2026-0258)
  advisories, replaced the unmaintained `bincode` (RUSTSEC-2025-0141), and removed the
  unsound `lru` 0.16 (RUSTSEC-2026-0253) by disabling turso's unused full-text search.

## [0.1.0] - 2026-09-25

First public release. WeftDB is pre-beta: the API will change, and the version
number says so.

### Added

- **Interpolation as a first-class query.** Ask for any resolution and get back a
  continuous series, reconstructed with the spline method you choose (linear,
  quadratic, cubic, polynomial), computed on SIMD/parallel CPU or GPU (`wgpu`).
- **Provenance labelling.** Every returned point is marked `raw`, `interpolated` or
  `extrapolated`, so a synthetic value is never silently mistaken for an observed one.
- **Declared precision.** Values are logically `BigDecimal`. Each aspect declares a
  physical encoding (`F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`,
  `BigDecimalText`) and an error bound; a value the encoding cannot represent within
  that bound is rejected rather than quietly rounded.
- **Typed columnar segment store** (`.weftseg`) on the measurement hot path, with
  bit-packing, delta-of-delta timestamp coding and a transposed value layout, over a
  Turso (libSQL) control plane for catalog, metadata and the segment index.
- **Downsampling** with mergeable partial reductions (`.weftpart` sidecars), including
  time-weighted averages, over an epoch-aligned bucket grid.
- **Apache Arrow interchange** — sealed segments to `RecordBatch` and back, plus Arrow
  IPC and Parquet bytes.
- **HTTP API** (`weft-server`) covering health/readiness, ingest, query, interpolation,
  storage management, backup/restore and a restore drill, with OpenTelemetry tracing.
- **Terminal UI** (`weft-tui`) for exploring and administering an instance.
- **Weft-Bench** (`weft-bench`), a reproducible, correctness-gated benchmark harness.
- **Analytics pipeline** (`weft-orchestration`) chaining batching, pattern extraction,
  event detection, correlation and signal generation.
- Pre-built `weft-server`, `weft-tui` and `weft-bench` archives for Linux (x86_64 and
  aarch64), macOS (x86_64 and arm64) and Windows (x86_64), with a `SHA256SUMS` file,
  attached to the GitHub release.

### Changed

- **The workspace now builds on stable Rust** (1.95+). The `#![feature(stmt_expr_attributes)]`
  gate is gone, so nightly is no longer required to build, test or depend on any crate.
  Only `cargo fmt` still uses nightly, because `rustfmt.toml` sets nightly-only options.
- **Crates renamed for publication.** The core crate is now `weftdb` (was `database`,
  a name already taken on crates.io) and the pipeline crate is `weft-orchestration`
  (was `database_orchestration`). Import paths change accordingly:
  `use database::…` becomes `use weftdb::…`, and `use database_orchestration::…`
  becomes `use weft_orchestration::…`.

### Internal

- `splimes`' unit-test tree is now gated behind `#[cfg(test)]` instead of compiling
  into released library builds.
- Dropped unused `splimes` dependencies (`flume`, `num_cpus`, `futures`,
  `futures-channel`, `pollster`, and `rand` outside dev builds).

[Unreleased]: https://github.com/basic-automation/weftdb/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/basic-automation/weftdb/releases/tag/v0.1.0
