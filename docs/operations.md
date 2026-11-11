# Operations

This document covers the operational procedures and failure modes a WeftDB
deployment needs. For the HTTP API reference, configuration variable catalogue,
storage internals and benchmarks, see the [reference](reference.md).

---

## Backup and restore

The [reference](reference.md#backup--restore) documents the backup and restore
surface in full — the online snapshot, the drill route, the backup daemon with
retention, and the `weftdb::restore_control_plane` library function. This
section covers the operational shape.

### What a backup consists of

A backup is a directory whose name is its label, containing the control plane's
four SQLite databases (catalog, schema, segment index, metadata rollup) and a
`MANIFEST.json`. The daemon names its backups `backup-<unix_millis>`; the API
lets you name yours with `?label=`.

The `.weftseg` measurement frames are **not** part of a control-plane snapshot.
A **whole-store** backup (only possible through the CLI `weft-server restore`,
which reads the manifest to decide which library function to call) also carries
a `segments/` directory of frames.

A backup appears on disk only once it is complete and verified: it is built in a
`.partial-<label>-<nonce>/` staging directory, verified, fsynced, renamed to its
label, and the parent fsynced. A crash before the rename leaves only the
`.partial-*` directory, which is never counted or restorable.

### Online backup (HTTP)

```bash
# Take an online snapshot (source verification — quiescent store recommended)
curl -X POST http://127.0.0.1:8080/api/v1/storage/backup

# Take a snapshot with snapshot-only verification (safe under concurrent writes)
curl -X POST 'http://127.0.0.1:8080/api/v1/storage/backup?verify=snapshot'

# Take a snapshot with a label
curl -X POST 'http://127.0.0.1:8080/api/v1/storage/backup?label=nightly-01'
```

The repository provides no backup listing endpoint. List the backup directory
directly on the server:

```bash
ls -1 /var/lib/weftdb/backups/
```

### Unattended backup daemon

Set two environment variables to enable it:

| Variable | Default | Effect |
|---|---|---|
| `WEFT_BACKUP_INTERVAL_SECS` | Unset (daemon off) | Seconds between daemon ticks |
| `WEFT_BACKUP_KEEP` | 0 (no pruning) | Newest N daemon backups to retain |

The daemon ticks with `MissedTickBehavior::Skip` (no catch-up) and verifies
snapshot-only. It prunes only daemon-generated `backup-<digits>` directories
that it counts; API-labelled snapshots and manually-renamed directories are
never pruned. Pruning runs only after a successful snapshot, so a run of
failures cannot delete the last good backup.

### Restore (CLI)

`weft-server restore` is the offline restore command, for when the server
cannot serve:

```bash
# Restore a control-plane backup into a fresh root
weft-server restore /var/lib/weftdb/backups/backup-1744246800000 \
  --into /var/lib/weftdb/store

# Restore and adopt (open the root once to adopt frames)
weft-server restore /path/to/backup --into /mnt/restored \
  --adopt --database mydb --subject host-1
```

The command reads the backup's manifest and picks the correct restore function:
a whole-store backup (control plane + frames) goes through
`weftdb::restore_store`; a control-plane-only backup through
`restore_control_plane`. It exits `78` (fix the command line or the destination)
or `65` (the backup needs an operator).

### Verify restorability without interrupting service

```bash
curl -X POST \
  'http://127.0.0.1:8080/api/v1/storage/restore/drill?label=backup-1744246800000'
```

This rehearses a restore into a throwaway directory, reports `restorable`, and
cleans up. Non-destructive by construction.

---

## systemd

The server can be run as a systemd service using the unit file at
[`packaging/systemd/weft-server.service`](../packaging/systemd/weft-server.service).

### What the unit enforces

- **`Type=exec`** — the binary must start successfully; a missing interpreter
  or a glibc mismatch produces a start failure, not a silent success.
- **`ProtectHome=yes`** — `/home` is absent from the service's view (the
  binary cannot fall back to `~/.weftdb/data`). The environment file must
  supply `WEFT_DATA_DIR` explicitly.
- **`ProtectSystem=strict`** — `/usr`, `/etc`, `/boot` are read-only. Only
  `/var/lib/weftdb` is writable (via `ReadWritePaths`), and only because
  `StateDirectory=weftdb` is declared. When `PrivateTmp=yes` is set alongside
  strict system protection, the store needs a writable temporary directory set
  via `TMPDIR` inside the read-write paths:
  `Environment=TMPDIR=/var/lib/weftdb/tmp`.
- **`MemoryDenyWriteExecute=yes`** — enforced. The server does not JIT, call
  `dlopen`, or embed a wasm engine. GPU backends work with this restriction
  (the NVIDIA Vulkan driver compiles SPIR-V under W^X).
- **`PrivateDevices` deliberately off** — the GPU needs `/dev/dri` visible.
- **`LimitNOFILE=65536`** — systemd's default soft limit of 1024 would cap
  connections at ~512.
- **`TimeoutStopSec=90`** — the graceful shutdown drain (SIGTERM +
  `WEFT_SHUTDOWN_GRACE_SECS`, default 5 s) must finish within this budget.
- **Maintenance budget** is the 90-second `TimeoutStopSec`: after SIGTERM,
  active connections get their grace window, the task drain runs, and the
  store closes. If `WEFT_SHUTDOWN_GRACE_SECS` is raised above ~60 s,
  `TimeoutStopSec` must be raised with it.

### Required paths

| Path | Created by | Purpose |
|---|---|---|
| `/var/lib/weftdb` | `StateDirectory=weftdb` (systemd) | Data directory, store root (`/var/lib/weftdb/store`), backups (`/var/lib/weftdb/backups`) |
| `/run/weftdb` | [`weftdb.tmpfiles`](../packaging/systemd/weftdb.tmpfiles) + `systemd-tmpfiles --create` | Runtime directory for API key files, survives service restart |
| `/etc/weftdb/weft-server.env` | Install script (created once, never overwritten) | Operator configuration (all `WEFT_*` variables) |
| `/etc/systemd/system/weft-server.service.d/` | Operator (drop-in overrides) | Override snippets — package updates never touch this directory |

### Installation

The support files are at `packaging/systemd/`:

- `weft-server.service` — the unit
- `weftdb.sysusers` — creates the `weftdb` user and group
- `weftdb.tmpfiles` — creates `/run/weftdb`

The install script at
[`scripts/packaging/install-host.sh`](../scripts/packaging/install-host.sh)
places everything in the right locations and prints the four post-install
commands. It never overwrites an existing environment file.

```bash
# Install from a release archive or a build tree
sudo scripts/packaging/install-host.sh

# After the script finishes, run its printed commands:
sudo systemd-sysusers
sudo systemd-tmpfiles --create
sudo systemctl daemon-reload
sudo systemctl enable --now weft-server
```

The four commands after the install are what
[`scripts/packaging/test-host-install.sh`](../scripts/packaging/test-host-install.sh)
proves on a throwaway tree in CI.

### Why ProtectHome=yes matters during install

Because the unit masks `/home`, the install script
[`scripts/packaging/install-host.sh`](../scripts/packaging/install-host.sh) does
not exec `weft-server` from the unit while the binary lives on the home
partition. Instead it copies the binary to the install prefix first — the
operator stages the binary, the unit finds it at the compiled-in path
(`/usr/local/bin/weft-server` by default), and the script dispatches
`ExecStart`-rewrite to match the chosen prefix.

---

## Container

The [Running in a container](reference.md#running-in-a-container) section of
the reference covers the server image in detail — build, smoke test, required
keys file, TLS limitation on the health check, and the `docker run` command.
This section adds what an operator needs beyond that.

### Image publishing

The repository publishes multi-arch container images to
`ghcr.io/basic-automation/weftdb` on every
[release](https://github.com/basic-automation/weftdb/releases) (see
[`.github/workflows/publish-image.yml`](../.github/workflows/publish-image.yml)).
Each release tag becomes the image tag; the `:latest` tag is updated on
releases. The image is built for `linux/amd64` and `linux/arm64` on native
runners — no QEMU overhead. The amd64 leg runs a smoke test against the built
image before push; the arm64 leg is pushed without a separate smoke test (the
test suite runs on the combined manifest).

### Image details

- **Registry**: `ghcr.io/basic-automation/weftdb`
- **Runtime**: `debian:12-slim` (glibc 2.36, floor 2.28)
- **Server user**: uid 10001 (`weft`)
- **Port**: `8080/tcp`
- **Volume**: `/var/lib/weftdb`
- **Health check**: `GET /ready` via bash `/dev/tcp` — does **not** speak TLS
  (disable with `--no-healthcheck` when TLS is configured)
- **Labels include**: `ai.weftdb.glibc-floor` reflecting the binary's linked
  glibc version

### Pull and run

```bash
docker pull ghcr.io/basic-automation/weftdb:<tag>

# With a keys file (required for non-loopback bind) and a named volume
mkdir -p keys data && sudo chown 10001:10001 keys data
# ... create keys/api-keys.json ...
docker run -p 8080:8080 \
  -v "$PWD/keys:/run/weftdb:ro" \
  -v weftdb-data:/var/lib/weftdb \
  -e WEFT_KEYS_FILE=/run/weftdb/api-keys.json \
  ghcr.io/basic-automation/weftdb:<tag>

# Behind a gateway that owns authentication
docker run -p 8080:8080 \
  -e WEFT_AUTH=disabled \
  ghcr.io/basic-automation/weftdb:<tag>
```

### Build locally

```bash
scripts/packaging/build-image.sh --tag weftdb:local
scripts/packaging/smoke-container.sh --image weftdb:local
```

---

## Upgrade

The [upgrade notes](upgrade-notes.md) document the known upgrade paths (at this
point, v0.2.1 → v0.3.0). A changelog-driven document (`UPGRADING.md`) lives at
the repository root and is written as part of each release — read it before
upgrading.

### Operational shape

Upgrading a WeftDB host installation follows this pattern:

1. **Read the upgrade notes and the per-release `UPGRADING.md`** for any
   breaking changes, new configuration variables, or migration instructions.
2. **Stop the service.**

   ```bash
   sudo systemctl stop weft-server
   ```

3. **Take a backup of the data directory.** On a host install the data lives at
   `/var/lib/weftdb` by default. A full directory copy (or an online snapshot
   taken before stopping) covers the control plane. For a container deployment,
   snapshot the volume.
4. **Replace the binary** (or pull the new container image).
5. **Review environment variables.** The `weft-server` registry rejects
   unrecognised `WEFT_*` names at startup. Check the release notes and the
   new env template at `/usr/share/doc/weftdb/weft-server.env` for new or
   changed variables.
6. **Start the service.**

   ```bash
   sudo systemctl start weft-server
   ```

7. **Verify.** Check the journal for a clean start, confirm `GET /ready`
   returns `200`, and run a drill restore or an ingest-read round-trip.

There is no database migration command to run; the store applies additive
migrations on first open. When the upgrade notes say "no store migration",
that is tested and the store format (layout number, floor) is unchanged.

---

## Failure modes and what to do

### Disk full

A full disk is a refusal, not a crash. Every ingest and seal reserves
`WEFT_DISK_RESERVE_BYTES` (default 256 MiB) of free space and is answered
`507 insufficient_storage` when it cannot leave that much — with nothing
written, so a refusal never leaves a half-written frame behind.

The refusal happens at the first point where the server can see the cost:

- **before the body is read**, against the declared `Content-Length` (the body
  cap when none is declared), so a request the disk cannot take costs no
  bandwidth;
- **after decode**, against what the decoded columns actually weigh, which is
  what a compressed body's declared length cannot tell you;
- **at the seal**, by the store's own guard, on the bytes the frame will
  really write.

The same code answers a filesystem that refuses mid-write: an `ENOSPC` (or
`ERROR_DISK_FULL`) surfacing from the operating system is classified
`507 insufficient_storage` rather than a bare `500`. So an operator sees one
code whether the guard predicted the shortage or the filesystem reported it.

Two deliberate gaps. Maintenance (compaction, reconciliation) is **not**
guarded — it is how a store gets space back, and refusing it would deadlock
the recovery. And where the operating system cannot describe free space the
guard has nothing to check against: it lets the write through, the filesystem
answers, and `disk_refusing` on `GET /ready` stays `false`.

What to watch, and what to do:

```bash
df -h /var/lib/weftdb
```

- `GET /api/v1/storage/headroom` — `total_bytes`, `available_bytes`,
  `reserve_bytes`, `writable_bytes` (what a write may still take) and
  `refusing` (the guard is refusing every write now).
- `GET /ready` — `disk_refusing` is `true` while every ingest and seal is
  refused. Reads keep working, so `ready` stays `true`: a full disk is a
  write outage, not an outage.
- `/metrics` — `weft_disk_total_bytes`, `weft_disk_available_bytes`,
  `weft_disk_reserve_bytes`, `weft_disk_writable_bytes`, and
  `weft_disk_guard_refusing` (`1` while every write is refused). Alert on
  `weft_disk_writable_bytes` crossing zero, not on the 507 rate.

A refused ingest is **not** recorded against its `Idempotency-Key`, so the
client's retry succeeds once there is room; nothing has to be replayed by
hand. To get room back: free space, let maintenance compact, or lower
`WEFT_DISK_RESERVE_BYTES` — it is read once at startup, so a change needs a
restart. A disk that is full enough to refuse maintenance as well is the one
case that needs operator action beyond freeing space: the journal shows the
file-system-level errors.

### Service account and permissions

The server runs as `weftdb` (uid/gid from
[`weftdb.sysusers`](../packaging/systemd/weftdb.sysusers)). The data directory
(`/var/lib/weftdb`, created by `StateDirectory`) is owned by
`weftdb:weftdb`. A common initial failure is a bind-mounted directory with the
wrong owner: the store's first act is to probe the root for write access.
Check the journal:

```bash
journalctl -u weft-server --since "5 min ago"
```

### Configuration errors

The `weft-server` registry (PAC-5b) rejects unrecognised `WEFT_*` names at
startup and exits `78`. A variable name that is one character off, or a value
outside its declared shape, is caught before anything is served. The journal
reports the variable name and why it was refused.

### Store errors

A store that fails to open exits `65` (store needs operator action). Causes
include:

- A layout or `min_write_layout` newer than the binary
- An unreadable `STORE_FORMAT`
- A scope mismatch (the environment names a database/subject the store does
  not contain)
- An incomplete restore
- Frames without a control-plane trailer

### Backup failure

The daemon's backup failures are recorded in metrics
(`weft_backup_failures_total`). A transient failure (network filesystem glitch,
short disk stall) causes the daemon to skip the tick and retry on the next
period. A repeated failure is visible in the metric and the journal.

### Logs and metrics

**Logs.** The server uses the [`tracing`](https://docs.rs/tracing) framework.
Set `RUST_LOG` to control verbosity (defaults to `error` or the distribution's
default). Every request gets a root span
(`request{method,path,request_id}`) with nested per-stage spans.

```bash
journalctl -u weft-server -f
```

For a container:

```bash
docker logs -f <container>
```

OpenTelemetry export is available by setting
`OTEL_EXPORTER_OTLP_ENDPOINT` (to an OTLP/gRPC collector), in addition to
the journal output.

**Metrics (`GET /metrics`).** The following backup-related metrics are
exposed at `/metrics` in the Prometheus text exposition format:

| Metric | Type | Meaning |
|---|---|---|
| `weft_backup_snapshots_total` | counter | Successful control-plane snapshots |
| `weft_backup_bytes_written_total` | counter | Total bytes written by snapshots |
| `weft_backup_failures_total` | counter | Daemon ticks that failed to snapshot |
| `weft_backup_last_success_seconds` | gauge | Unix seconds of most recent verified snapshot (0 when none) |
| `weft_backup_duration_seconds` | histogram | Wall-clock duration of each completed snapshot |

Other notable metrics:

| Metric | Type | Meaning |
|---|---|---|
| `weft_interpolate_requests_total` | counter | Interpolation requests received |
| `weft_interpolate_errors_total` | counter | Interpolation errors |
| `weft_ingest_requests_total` | counter | Ingest requests received |
| `weft_ingest_rows_sealed_total` | counter | Rows written to sealed segments |
| `weft_reconcile_passes_total` | counter | Out-of-order reconciliation passes |
| `weft_reconcile_failed_passes_total` | counter | Aspect sweeps that failed |
| `weft_interpolate_duration_seconds` | histogram | End-to-end interpolation latency |
| `weft_ingest_duration_seconds` | histogram | Ingest seal-path latency |
| `weft_backup_duration_seconds` | histogram | Backup snapshot duration |

Scrape `/metrics` every 15–30 s for the latency histograms to remain
meaningful. The `weft_backup_last_success_seconds` gauge is the health signal
for the backup daemon — alert when it is more than
`2 × WEFT_BACKUP_INTERVAL_SECS` behind the current time.

### Poison exit

When `WEFT_ON_AMBIGUOUS_COMMIT=exit` is set, a commit whose outcome is
unknown exits `70` (poison exit) instead of continuing. This is an extreme
setting for deployments that must *know* every write committed — it stops the
server rather than risk serving from an uncertain store. The journal names the
aspect and the frame at the time of the ambiguity. Restart after investigating;
the store's open-recovery sorts out the frames.
