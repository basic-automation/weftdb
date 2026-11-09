# Security

**SEC-14** · Part of the 1.0 release plan (roadmap W8). This document describes the
authentication, authorisation, transport, audit, and exposure model of `weft-server`
at v0.3.0.

Every statement below is traceable to the source file and line that enforces or documents
it, in the style `path:line` (or `path:symbol` for a function/type).

---

## Contents

- [TLS](#tls)
- [Authentication](#authentication)
- [Authorisation (roles and scopes)](#authorisation-roles-and-scopes)
- [Credential-in-query guard](#credential-in-query-guard)
- [Proxy guard (auth-off)](#proxy-guard-auth-off)
- [Exposure matrix](#exposure-matrix)
- [Exit statuses](#exit-statuses)
- [Startup enforcement](#startup-enforcement)
- [Host guard](#host-guard)
- [Audit log](#audit-log)
- [Connection management](#connection-management)
- [Key management and rotation](#key-management-and-rotation)
- [Container image](#container-image)
- [Denial-of-service ceilings](#denial-of-service-ceilings)
- [What the product does NOT do](#what-the-product-does-not-do)
- [Tracked issues](#tracked-issues)

---

## TLS

`WEFT_TLS_CERT_FILE` and `WEFT_TLS_KEY_FILE` name PEM files for the TLS listener. Naming one
without the other stops the server from starting (`weft-server/src/tls.rs:140-143`,
`TlsConfig::from_env_values` returns `None` only when both are absent). When both are set, the
HTTP listener serves TLS only. Unset → plaintext, and the C8 gate below applies.

TLS is **never required** by the server (the library crate `weftdb` has no TLS
dependency), and the deployment story is that TLS is off until these two variables are
set.

### C8 gate — plaintext (`weft-server/src/tls.rs:206-250`, `PlaintextGate::check`)

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

### API keys (`weft-server/src/auth.rs`)

`WEFT_KEYS_FILE` names a JSON file of API keys. Keys are Bearer tokens sent as
`Authorization: Bearer <key>` (`auth.rs:4`). The keys file is read once at startup and
reloaded on `SIGHUP` (`auth.rs:52-53`, SEC-7; `docker kill --signal=HUP weftdb`).

Format (`weft-server keygen` writes this; see `auth.rs:16-23`, `KeyEntry`):

```json
{
  "keys": [
    { "id": "grafana-dash", "key": "sha256:<64 hex digits>", "role": "reader" },
    { "id": "batch-ingest", "key": "sha256:<64 hex digits>", "role": "writer" }
  ]
}
```

- `id` — human label, surfaced in audit events and rate-limit diagnostics.
- `key` — **SHA-256 hash** of the bearer secret, prefixed `sha256:`. The raw secret is
  printed once by `weft-server keygen` and never stored.
- `role` — frozen role name from the six-role table below
  (`auth.rs:33-36`, `routes.rs:133-152`).

Key reload (`SIGHUP`) atomically swaps the keys in force (`auth.rs:395-410`,
`SharedKeyStore::reload`). A malformed edit is refused and the keys in force stay; the
server logs the refusal and keeps serving with the old set.

Every minted key carries the `weft_` prefix (`auth.rs:168`, `TOKEN_PREFIX`), so it is
recognisable in a log line or a password manager as a WeftDB key. The prefix is part of
the hashed text: a key authenticates exactly as printed, prefix included.

### Auth modes (`WEFT_AUTH`, `auth.rs:110-117`)

| Value | Behaviour |
|-------|-----------|
| unset | Authentication is **on** when `WEFT_KEYS_FILE` names a mounted keys file, **off** otherwise. |
| `keys` | Requires the keys file. The server refuses to start if `WEFT_KEYS_FILE` is unset or the file cannot be parsed. |
| `disabled` | Authentication is explicitly disabled — for deployments that terminate TLS and authenticate at a gateway (C8, decision 7). |

An unauthenticated request to a keys-guarded server is answered `401 unauthorized` with
`WWW-Authenticate: Bearer realm="weftdb"` (`auth.rs:172-173`,
`auth.rs:683-688`). The
`WEFT_AUTH` and `WEFT_KEYS_FILE` settings are read once at startup (with the exception of
the `SIGHUP` key refresh).

### The auth middleware (`weft-server/src/auth.rs:770-791`)

The middleware `enforce`, installed by `crate::app_with_state`, runs after the route
gate. The decision sequence is:

1. The route's required scope is looked up from the registry
   (`routes.rs`, `RouteTable`).
2. If the scope is `Probe` (probe-only routes like `/health`, `/ready`), the request
   passes with no credential check (`auth.rs:774`).
3. Otherwise:
   - No `Authorization` header → `401` (`auth.rs:777`).
   - A malformed header or an unknown key → `401` with the challenge
     (`auth.rs:685`, `presented` function).
   - A key whose role lacks the required scope → `403 forbidden`, naming the key id,
     role and the scope the route needs (`auth.rs:789`).
   - A valid key with the required scope → passes with the [`Principal`]
     (`auth.rs:195-200`) in the request extensions, for audit and downstream
     layers.

A request the registry itself refuses (an unregistered path, a wrong method) is left to
the gate and the fallback: the auth layer defers whenever its `(method, path)` lookup
finds no entry, so a `404`/`405` keeps its shape whether or not auth is on
(`auth.rs:109-110`).

## Authorisation (roles and scopes)

Six frozen roles are defined in `weft-server/src/routes.rs:133-152`. Each role grants a
cumulative set of scopes; every route in the registry declares the one scope it needs
(`routes.rs:54-100`).

| Role | Frozen name | Grants scopes | Can do |
|------|-------------|---------------|--------|
| **`anonymous`** | `anonymous` | Probes only | `/health`, `/ready`, `/healthz`, `/readyz` |
| **`reader`** | `reader` | Probe, Read, Compute | Stored reads, stateless compute (interpolate, downsample) |
| **`writer`** | `writer` | Probe, Read, Compute, Ingest | Reader + ingest points in every format |
| **`declarer`** | `declarer` | Probe, Read, Compute, Ingest, Declare | Writer + declare aspect schemas |
| **`maintainer`** | `maintainer` | Probe, Metrics, Read, Compute, Ingest, Declare, Maintain | Declarer + store maintenance, `/metrics` |
| **`admin`** | `admin` | All 8 scopes | Everything, including backup |

The frozen scope table has eight entries (`routes.rs:54-100`):
`Probe`, `Metrics`, `Read`, `Compute`, `Ingest`, `Declare`, `Maintain`, `Backup`.

A request with an unknown role name in the keys file is refused as a configuration
error at startup (`auth.rs:272`, `role_from_name` at `auth.rs:422-423`). A key
assigning the `anonymous` role is also refused, because it grants nothing beyond the
public probes and a key that cannot authenticate is a configuration mistake
(`auth.rs:273-274`).

The role→scope mapping is enforced by `Role::allows` (`routes.rs:160-162`):
`role.scopes().contains(&scope)`. The grant is cumulative by construction
(`routes.rs:141-154`).

### Probe routes

`/health`, `/ready`, `/healthz`, `/readyz` are always unauthenticated (anonymous), even
when auth is on. The admin listener (when `WEFT_ADMIN_ADDR` is set) serves only these
routes, with no auth (`auth.rs:115-116`, scope `Probe`).

## Credential-in-query guard (`weft-server/src/auth.rs:178-187, 653-662`)

A credential in the query string is refused `400 credentials_in_query` on any route,
in **every** authentication mode (keys mounted, off, or explicitly disabled). The
parameter names checked are:

```
api_key, apikey, access_token, auth_token
```

(`auth.rs:187`, `CREDENTIAL_QUERY_PARAMS`). The check runs as a layer in
[`app_with_state`](crate::app_with_state), after the route gate. Only the
parameter *name* is read; values are never logged or echoed (`auth.rs:661-662`).

This prevents credentials from leaking into access logs, proxy logs, and browser
history. It applies regardless of whether the server would have accepted the key
(`auth.rs:110`).

## Proxy guard (auth-off) (`weft-server/src/auth.rs:110-113`)

While authentication is off *implicitly* (no keys file mounted, `AuthConfig::Off`), a
request carrying a proxy's `Forwarded` or `X-Forwarded-*` header is refused
`403 proxied_request_requires_auth`. The checked header names are
(`auth.rs:694-698`, `PROXY_HEADERS`):

```
forwarded, x-forwarded-for, x-forwarded-host, x-forwarded-proto
```

A forwarded header means the caller the server sees is not the caller that sent the
request, so the loopback guard's address-based protection is bypassable. When auth is
on, forwarded headers are ignored (the key authenticates the caller).

## Exposure matrix

SEC-8 (`weft-server/src/tls.rs:25-40`) defines the combinations of bind address, TLS and
authentication that `weft-server` accepts at startup and how they are guarded. The
decision table is implemented in `PlaintextGate::check` (`tls.rs:206-250`) and
`HostGuard::for_bind` (`host_guard.rs:81-88`).

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
both), or it refuses to start.

## Exit statuses

`weft-server` uses the plan C7 exit-code table (`weft-server/src/cli.rs:40-49`,
`settings.rs:300-323`):

| Exit status | Meaning | Variable | Route |
|-------------|---------|----------|-------|
| 1 | Unexpected error — no other row classifies the failure. | `UNEXPECTED_EXIT_CODE` | `settings.rs:323` |
| 2 | Usage error — the command line itself is wrong (unknown command, unknown option, missing value). | `USAGE_EXIT_CODE` | `settings.rs:321` |
| 65 | Store error — the store needs operator action (layout newer than binary, unreadable `STORE_FORMAT`, scope mismatch, incomplete restore). | `STORE_EXIT_CODE` | `settings.rs:309` |
| 75 | Root locked — the store root is already locked by another process. | `ROOT_LOCKED_EXIT_CODE` | `settings.rs:312` |
| 78 | Configuration error — invalid configuration, unsupported deployment, or filesystem refusal. This is what the C8 gate, the strict configuration phase, and a missing keys file in `keys` mode produce. | `CONFIG_EXIT_CODE` | `settings.rs:302` |

A poisoned-store ambiguous-commit mode can also exit with status 70
(`WEFT_ON_AMBIGUOUS_COMMIT=exit`, `settings.rs:138`, `store_open.rs`).

The server's `main` dispatch (`main.rs:26-28`) summarises: "a usage error exits 2, a
configuration refusal exits 78, a store that needs operator action 65, a locked root 75,
a poison exit 70 — and anything no phase classified falls out as 1."

## Startup enforcement

`weft-server` validates its environment during startup, before any request is served:

- **Strict configuration** (PAC-5b, `main.rs:33-37`): An unrecognised `WEFT_*` name (a
  typo such as `WEFT_BACKUP_DIRS`) or a value that its reader cannot parse stops the
  server with exit status **78** (`CONFIG_EXIT_CODE`). The registry is at
  `weft-server/src/settings.rs`.
- **Plaintext gate** (`tls.rs:238-249`): A non-loopback bind without TLS and without an
  opt-in is refused with status **78**, as described in the [exposure matrix](#exposure-matrix).
- **TLS cert/key pair** (`tls.rs:140-143`): Exactly one of `WEFT_TLS_CERT_FILE` /
  `WEFT_TLS_KEY_FILE` set is a startup error (status **78**).
- **Keys file** (`auth.rs:260-280`): In `WEFT_AUTH=keys` mode, a missing or unparseable
  keys file stops the server (status **78**). A file with unknown roles, duplicate ids,
  shared hashes, or the `anonymous` role is refused the same way.
- **Store open** (`main.rs:48`): A store that cannot be opened exits **65**
  (`STORE_EXIT_CODE`). A locked root exits **75**.
- **Audit file** (`audit.rs:86-88`): A server that cannot open the audit file at startup
  refuses to start. A torn record at the end of an existing file is also refused.
- **Admin listener** (`config-schema.md`, `WEFT_ADMIN_ADDR`): If set to an unparseable
  address, stops the server from starting (status **78**).

What the server does NOT check at startup:

- Whether the keys file is readable by the server's uid (it is read, and a permission
  error surfaces immediately as a startup failure).
- Whether network addresses are reachable (the bind itself fails, not a pre-flight check).
- Whether the audit file path is writable (the open succeeds or fails on its own terms).

## Host guard

The loopback request guard (`weft-server/src/host_guard.rs:1-43`, `HostGuard::Loopback`)
prevents web pages and DNS rebinding from reaching a loopback-bound server.

On a loopback bind, the guard refuses:

- **A request not addressed to a loopback host**, with `403 Forbidden` and error code
  `host_not_allowed` (`host_guard.rs:11-12`). The `Host` header must be `localhost`, an
  IPv4 address in `127.0.0.0/8` or `[::1]` (or the IPv4-mapped `[::ffff:127.0.0.1]`),
  each with an optional port. A request with no host or more than one is refused.
- **A state-changing request from a non-loopback web origin**, with `403 Forbidden` and
  error code `cross_origin_write` (`host_guard.rs:21-22`): any method but `GET`, `HEAD`,
  `OPTIONS` and `TRACE` that carries an `Origin` header whose host is not a loopback
  host, including the opaque `null` origin. Curl and other non-browser clients send no
  `Origin` and are not affected.

`WEFT_ALLOW_ANY_HOST` turns this guard off (`host_guard.rs:57-60`,
`host_guard.rs:86-88`). The guard is off for a non-loopback bind
(`host_guard.rs:37-39`).

## Audit log

`WEFT_AUDIT_FILE` names the fsynced append-only JSONL audit log (`weft-server/src/audit.rs:1-110`,
SEC-9, decision 8). Every admin or mutating operation — ingest, declare, maintenance,
backup — gets:

1. An **attempt** record before the operation runs (`audit.rs:350-359`, `AuditLog::attempt`).
2. An **outcome** record after (success or failure, with the error) (`audit.rs:376-383`,
   `AuditLog::outcome`).

The operation is refused `503 audit_unavailable` when either record cannot be written
(fail-closed, `audit.rs:487-498`). Committed ingests, authentication denials and daemon
panics land as `weft_audit` events in the same file (`audit.rs:407`, `AuditLog::event`).

When unset, the file lives at `<store root>/audit.jsonl` (or `./weft-audit.jsonl` with no
store configured). A server that cannot open the audit file at startup refuses to start.

### Record format (`audit.rs:58-75`)

```json
{"v":1,"seq":41,"ts_ms":1762137600000,"kind":"attempt","operation":"storage_aspect_declare","method":"POST","path":"/api/v1/storage/aspects","principal":"operator","role":"admin"}
{"v":1,"seq":42,"ts_ms":1762137600123,"kind":"outcome","audit_id":41,"operation":"storage_aspect_declare","status":201,"result":"ok","duration_ms":7}
{"v":1,"seq":43,"ts_ms":1762137600456,"kind":"event","event":"ingest","aspect":"price","format":"json","rows":120,"segments":1,"commit_epoch":9,"series_id":0,"keyed":true}
```

- `seq` is a per-file monotonic counter. Opening an existing file counts its lines and
  continues the sequence.
- `ts_ms` is wall-clock milliseconds since the Unix epoch.
- A record never carries a credential: the principal is the key's *id* and role
  (`audit.rs:74-75`), and no request body, header value or key material is written.

### Write durability (`audit.rs:78-83`)

A write is `write_all` of the whole line followed by `sync_data`. The first failed write
poisons the log: the failure is latched and every later write fails with the same message.
Opening a file whose last line is not `\n`-terminated — the torn tail of a crash mid-write —
is refused (`audit.rs:81-83`).

### Shutdown (`audit.rs:105-110`)

The drain order is serve → **SEC-9 TaskTracker** (`audit.rs:108`) → `store.shutdown()` →
runtime `shutdown_timeout(5 s)`. The tracker is `AuditLog::spawn`'s: every audited handler
runs through it, and `AuditLog::close_and_drain` waits for in-flight audits before the
store shuts down.

## Connection management

All connection limits and deadlines are enforced **before authentication** (SEC-4,
`weft-server/src/serve.rs`).

| Ceiling | Variable | Default | Effect |
|---------|----------|---------|--------|
| Max concurrent connections | `WEFT_MAX_CONNECTIONS` | Half of `RLIMIT_NOFILE`, 16..65,536 | A connection beyond the cap waits in the listen backlog. |
| Header read timeout | `WEFT_HEADER_READ_TIMEOUT_SECS` | 30 s | A peer that takes longer to deliver a complete request header is disconnected. |
| Keep-alive idle timeout | `WEFT_KEEP_ALIVE_TIMEOUT_SECS` | 30 s | An idle connection is closed. |
| Write stall timeout | `WEFT_WRITE_STALL_TIMEOUT_SECS` | 10 s | A peer that stops reading is disconnected. |

These are enforced by the hyper serve path before any application logic runs
(`tls.rs:8-10`: the TLS handshake also runs inside the connection task, under the
connection cap and the keep-alive deadline).

## Key management and rotation

- Keys are generated by `weft-server keygen --id <label> --role <role> --keys-file <path>`
  (`keygen.rs:7-22`). The raw secret is printed to stdout once.
- The keys file stores only the SHA-256 hash (`auth.rs:453-456`). A leaked keys file does
  not reveal the raw secrets — only a brute force over the hash (which is the same cost
  as guessing the secret directly).
- Reload with `SIGHUP` (`auth.rs:52-53`, SEC-7). A key can be added, removed or re-roled
  without restarting the server.
- A key is never present in a log line, an error message, or an audit record
  (`auth.rs:121-123`). The [`Principal`] carries only the key's id and role
  (`auth.rs:195-200`).
- There is no key expiry or automatic revocation API yet. Tracked as SEC-11.

## Container image

The [Docker image](reference.md#running-in-a-container) `weftdb`:

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
exhaustion. All are enforced in the request handler, after authentication.

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

## What the product does NOT do

These are security-relevant absences that an operator might assume are present:

- **No rate limiting.** There is no per-client or per-key request rate limiter. The
  connection cap (`WEFT_MAX_CONNECTIONS`) and the in-flight caps
  (`WEFT_QUERY_MAX_INFLIGHT`, `WEFT_INGEST_MAX_INFLIGHT_BYTES`) bound resource use, but
  a single key can fill the in-flight budgets as fast as the network and server permit.
  grep for `rate_limit` in `weft-server/src/` returns only the OpenAPI error-description
  schema, not an enforced limit.

- **No per-key scopes beyond the six frozen roles.** Every key with the `writer` role
  can ingest into every aspect. Per-aspect or per-route key grants are deferred past 1.0
  (plan §10, `auth.rs:36`).

- **No per-key idempotency namespace.** The ingest idempotency ledger is keyed by
  `(aspect, key)` with no principal component, so a replay by a second principal meets
  the first principal's ledger row (`auth.rs:138-141`).

- **No key expiry or automatic revocation.** Keys have no `Expires` field. Tracked as
  SEC-11.

- **No certificate-based client auth.** Tracked as SEC-13.

- **No separate `admin` scope.** Backup and maintenance use the `admin` role's grant of
  the `Backup` and `Maintain` scopes (all scopes, since `Admin` gets all eight). Tracked
  as SEC-12.

- **No Windows free-space measurement.** The `WEFT_DISK_RESERVE_BYTES` variable is
  registered in the config schema but the Windows implementation is not shipped (issue
  #667). On Windows, the disk guard is a no-op.

- **No audit log rotation.** The file grows unbounded; rotate it out-of-process
  (rename and `SIGHUP` to reopen). A torn record after a crash is detected at next
  startup and prevents the server from starting.

- **No `admin` listener auth.** The admin listener (when `WEFT_ADMIN_ADDR` is set) serves
  only `/health` and `/ready` with no authentication, even when auth is on for the main
  listener. The admin listener is intended for load-balancer probes only.

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