# Security

**SEC-14** · Part of the 1.0 release plan (roadmap W8). This document describes the
authentication, authorisation, transport, audit, and exposure model of `weft-server`
at v0.3.0.

---

## Contents

- [TLS](#tls)
- [Authentication](#authentication)
- [Authorisation (roles and scopes)](#authorisation-roles-and-scopes)
- [Exposure matrix](#exposure-matrix)
- [Host guard](#host-guard)
- [Audit log](#audit-log)
- [Connection management](#connection-management)
- [Key management and rotation](#key-management-and-rotation)
- [Container image](#container-image)
- [Denial-of-service ceilings](#denial-of-service-ceilings)
- [Tracked issues](#tracked-issues)

---

## TLS

`WEFT_TLS_CERT_FILE` and `WEFT_TLS_KEY_FILE` name PEM files for the TLS listener. Naming one
without the other stops the server from starting. When both are set, the HTTP listener
serves TLS only. Unset → plaintext, and the C8 gate below applies.

TLS is **never required** by the server (the library crate `weftdb` has no TLS
dependency), and the deployment story is that TLS is off until these two variables are
set.

### C8 gate — plaintext

A non-loopback plaintext bind is refused at startup unless either:

- TLS is configured (`WEFT_TLS_CERT_FILE` and `WEFT_TLS_KEY_FILE`), or
- `WEFT_AUTH=disabled` is set (opt-in for a gateway that authenticates), or
- `WEFT_ALLOW_PLAINTEXT` is set to a truthy value (opt-in acknowledgement).

A loopback bind (`127.0.0.1`, `::1`) is never refused in plaintext — the bind address
itself is the protection.

The container image binds `0.0.0.0:8080` and requires a keys file (see
[Container image](#container-image) below), which gates the non-loopback plaintext bind
through `WEFT_KEYS_FILE`.

## Authentication

### API keys

`WEFT_KEYS_FILE` names a JSON file of API keys. Keys are Bearer tokens sent as
`Authorization: Bearer <key>`. The keys file is read once at startup and reloaded on
`SIGHUP` (SEC-7; `docker kill --signal=HUP weftdb`).

Format (`weft-server keygen` writes this):

```json
{
  "keys": [
    { "id": "grafana-dash", "key": "<SHA-256 hash>", "role": "reader" },
    { "id": "batch-ingest", "key": "<SHA-256 hash>", "role": "writer" }
  ]
}
```

- `id` — human label, surfaced in audit events and rate-limit diagnostics.
- `key` — **SHA-256 hash** of the bearer secret. The raw secret is printed once by
  `weft-server keygen` and never stored.
- `role` — frozen role name. Two roles are defined (see [Authorisation](#authorisation-roles-and-scopes)).

Key reload (`SIGHUP`) atomically swaps the keys in force. A malformed edit is refused and
the keys in force stay; the server logs the refusal and keeps serving with the old set.

### Auth modes

`WEFT_AUTH` controls the authentication mode:

| Value | Behaviour |
|-------|-----------|
| unset | Authentication is **on** when `WEFT_KEYS_FILE` names a mounted keys file, **off** otherwise. |
| `keys` | Requires the keys file. The server refuses to start if `WEFT_KEYS_FILE` is unset or the file cannot be parsed. |
| `disabled` | Authentication is explicitly disabled — for deployments that terminate TLS and authenticate at a gateway (C8, decision 7). |

An unauthenticated request to a keys-guarded server is answered `401 unauthorized`. The
`WEFT_AUTH` and `WEFT_KEYS_FILE` settings are read once at startup (with the exception of
the `SIGHUP` key refresh).

## Authorisation (roles and scopes)

Two roles are currently defined:

| Role | Scope | Read | Write | Admin | Metrics |
|------|-------|------|-------|-------|---------|
| **`reader`** | Read-only data access. | ✅ stored reads, stateless compute | ❌ ingest, declare, maintenance | ❌ backup, key reload | ✅ `/metrics` |
| **`writer`** | Read and write data. | ✅ | ✅ | ❌ backup (`POST …/backup`) | ✅ |

A request with an unknown role is refused `403 forbidden`.

### Scope enforcement

Every route that touches stored data (storage reads, ingest, declare, maintenance)
enforces the `read`/`write` scope from the key's role. The scope decision is in
`weft-server/auth.rs` and the role→scope mapping is:

```
reader  → [read]
writer  → [read, write]
```

No route has a separate `admin` scope yet — that is tracked as SEC-12.

### Probe routes

`/health`, `/ready`, `/healthz`, `/readyz` are always unauthenticated (anonymous), even
when auth is on. The admin listener (when `WEFT_ADMIN_ADDR` is set) serves only these
routes, with no auth.

## Exposure matrix

SEC-8 defines the combinations of bind address, TLS and authentication that `weft-server`
accepts at startup and how they are guarded.

| Bind | TLS | Auth | Keys file | Result |
|------|-----|------|-----------|--------|
| Loopback (`127.0.0.1`) | Off | On or unset | Mounted | ✅ Serves. Auth enforced. |
| Loopback | Off | Off / `disabled` | Absent | ✅ Serves. Anonymous. |
| Loopback | On | Any | Any | ✅ Serves. |
| Non-loopback (`0.0.0.0`) | Off | On or unset | Mounted | ❌ **Refused** at startup (C8: plaintext non-loopback with auth=on is a protocol downgrade trap). See `WEFT_ALLOW_PLAINTEXT`. |
| Non-loopback | Off | `disabled` | n/a | ✅ Serves (opt-in, anonymous, plaintext — warned in logs). |
| Non-loopback | Off | unset | Absent | ❌ **Refused** (C8: non-loopback plaintext with no auth and no TLS). |
| Non-loopback | On | Any | Any | ✅ Serves. TLS terminates at server. |
| Non-loopback | On | On or unset | Mounted | ✅ Serves. Auth on TLS is the recommended deployment. |

The container image binds `0.0.0.0:8080`, so it requires either TLS or a keys file (or
both), or it refuses to start with status `78`.

## Host guard

On a loopback bind, the server answers only requests whose `Host` header is `localhost`,
`127.0.0.0/8` or `[::1]`. A request with any other `Host` is `421 misdirected_request`.
This prevents DNS rebinding attacks where an attacker's domain resolves to `127.0.0.1`.

`WEFT_ALLOW_ANY_HOST` turns this guard off — for a local reverse proxy that forwards a
different `Host`, or a browser UI whose `Origin` the guard would otherwise refuse.

State-changing requests (`POST`, `PUT`, `PATCH`, `DELETE`) from non-loopback web origins
are refused `403 forbidden` when the guard is on (unless the `Origin` is loopback too).
This prevents cross-site request forgery against the loopback-bound API.

A non-loopback bind is never guarded (the network boundary is the only protection).

## Audit log

`WEFT_AUDIT_FILE` names the fsynced append-only JSONL audit log (SEC-9, decision 8). Every
admin or mutating operation — ingest, declare, maintenance, backup — gets:

1. An **attempt** record before the operation runs.
2. An **outcome** record after (success or failure, with the error).

The operation is refused `503 audit_unavailable` when either record cannot be written
(fail-closed). Committed ingests, authentication denials and daemon panics land as
`weft_audit` events in the same file.

When unset, the file lives at `<store root>/audit.jsonl` (or `./weft-audit.jsonl` with no
store configured). A server that cannot open the audit file at startup refuses to start.

### Audit events

| Event | Trigger | Fields |
|-------|---------|--------|
| `ingest` | A successful ingest commit. | `aspect`, `rows`, `series_id`, `request_id` |
| `auth_denial` | A failed authentication attempt. | `key_id` (if presented), `remote`, `reason` |
| `declare` | An aspect declaration (create or re-declare). | `aspect`, `upsert_keys` |
| `maintenance` | A store-wide maintenance sweep. | `sweep` kind, aspects touched |
| `backup` | A control-plane backup. | `backup_dir`, `label` (if provided) |
| `panic` | A daemon panic / process crash. | signal, store UUID |
| `startup` | Server start. | build ID, store scope |
| `shutdown` | Graceful shutdown. | — |

The audit log is append-only; a torn record at the end (from a crash) causes the server to
refuse to start naming the file and what it expected vs. found. There is no audit log
rotation yet.

## Connection management

All connection limits and deadlines are enforced **before authentication** (SEC-4).

| Ceiling | Variable | Default | Effect |
|---------|----------|---------|--------|
| Max concurrent connections | `WEFT_MAX_CONNECTIONS` | Half of `RLIMIT_NOFILE`, 16..65,536 | A connection beyond the cap waits in the listen backlog. |
| Header read timeout | `WEFT_HEADER_READ_TIMEOUT_SECS` | 30 s | A peer that takes longer to deliver a complete request header is disconnected. |
| Keep-alive idle timeout | `WEFT_KEEP_ALIVE_TIMEOUT_SECS` | 30 s | An idle connection is closed. |
| Write stall timeout | `WEFT_WRITE_STALL_TIMEOUT_SECS` | 10 s | A peer that stops reading is disconnected. |

These are enforced by the hyper serve path before any application logic runs.

## Key management and rotation

- Keys are generated by `weft-server keygen --id <label> --role <role> --keys-file <path>`.
  The raw secret is printed to stdout once.
- The keys file stores only the SHA-256 hash. A leaked keys file does not reveal the raw
  secrets — only a brute force over the hash (which is the same cost as guessing the
  secret directly).
- Reload with `SIGHUP` (SEC-7). A key can be added, removed or re-roled without
  restarting the server.
- There is no key expiry or automatic revocation API yet. Tracked as SEC-13.

## Container image

The [Docker image](reference.md#running-in-a-container)`weftdb`:

- Binds `0.0.0.0:8080` (non-loopback), so the C8 gate requires a keys file or TLS.
- Runs as uid 10001 with `/var/lib/weftdb` as its volume.
- Has no shell, no package-manager state, no HTTP client.
- Health check reads `/ready` from inside the container.
- Published port is the firewall: `docker run --publish 127.0.0.1:8080:8080` is the
  recommended exposure.
- The health check does not speak TLS. A TLS-only container disables the built-in
  health check (`--no-healthcheck`).

## Denial-of-service ceilings

Beyond connection-level limits, several application-layer ceilings prevent resource
exhaustion:

| Ceiling | Variable | Default | Effect |
|---------|----------|---------|--------|
| In-flight read/compute requests | `WEFT_QUERY_MAX_INFLIGHT` | 128 | Past it: `503 overloaded` with `Retry-After`. Probes are never shed. |
| In-flight write bytes | `WEFT_INGEST_MAX_INFLIGHT_BYTES` | 256 MiB | Past it: `503 overloaded`. |
| Per-request ingest body cap | `WEFT_MAX_INGEST_BYTES` | 256 MiB | `413 payload_too_large`. |
| Per-request decoded row cap | `WEFT_MAX_INGEST_ROWS` | 10,000,000 | `413 too_many_rows`. |
| Per-request decoded byte cap | `WEFT_MAX_INGEST_DECODED_BYTES` | 1 GiB | `413 payload_too_large`. |
| Max stored-read page rows | `WEFT_MAX_PAGE_ROWS` | 100,000 | `422 result_too_large`. |
| Max stored-read window rows | `WEFT_MAX_READ_ROWS` | 5,000,000 | `422 result_too_large`. |
| Max downsample buckets | `WEFT_MAX_DOWNSAMPLE_BUCKETS` | 1,000,000 | `422 result_too_large`. |
| Max `at-multi` instants | `WEFT_MAX_AT_MULTI_INSTANTS` | 10,000 | `422 result_too_large`. |
| Max interpolate output points | `WEFT_MAX_INTERPOLATE_POINTS` | 10,000,000 | `400` naming the limit. |
| Stored read timeout | `WEFT_READ_TIMEOUT_SECS` | 30 s | `504 query_timeout`. |

## Tracked issues

Security items from the 1.0 plan that are open or in-progress:

| Issue | Title | Status |
|-------|-------|--------|
| SEC-7 | Key rotation (`SIGHUP`) | Shipped in v0.3.0 |
| SEC-8 | Exposure matrix and deployment guide | This document |
| SEC-9 | Audit log | Shipped in v0.3.0 (decision 8) |
| SEC-10 | OTEL endpoint redaction | Shipped in v0.2.0 |
| SEC-11 | Add `Expires` to keys | Not yet |
| SEC-12 | Admin access control (`admin` scope) | Not yet |
| SEC-13 | Certificate-based client auth | Not yet |
| SEC-14 | This document | v0.3.0 |