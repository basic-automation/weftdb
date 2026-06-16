# dsp-server

The benchmark-grade HTTP API surface for DSP (roadmap **Phase 2**). A commercial
time-series engine can't lead with an embedded Rust API plus a TUI: DSP-Bench and
external tooling (TSBS, Grafana, SDKs) need a stable HTTP surface to drive the
engine through. This crate is that surface.

Per the workspace hard constraints it carries **no vendor-specific
dependencies** — it is a thin HTTP shell over the DSP core (`splimes` for
interpolation, the shared `dsp-line-protocol` crate for ILP parsing) and never a
place for a concrete connector to leak in.

## Running

```sh
cargo run -p dsp-server
# dsp-server v0.1.0 listening on http://127.0.0.1:8080
```

The bind address defaults to `127.0.0.1:8080`; override it with `DSP_SERVER_ADDR`
(e.g. `DSP_SERVER_ADDR=0.0.0.0:9000`).

## Endpoints

| Method & path | Purpose |
|---------------|---------|
| `GET /health` | Liveness — the process is up. |
| `GET /ready` | Readiness — ready to accept traffic (gains real dependency checks as the control plane / segment store wire in). |
| `GET /metrics` | Prometheus text exposition of the server's counters. |
| `POST /api/v1/interpolate` | Interpolate a JSON point set onto a regular grid. |
| `POST /api/v1/interpolate/ilp` | Interpolate an InfluxDB Line Protocol payload. |
| `POST /api/v1/interpolate/point` | Evaluate the reconstructed signal at a single instant (interpolated vs extrapolated). |
| `POST /api/v1/downsample` | Reduce a JSON point set into grid-aligned aggregate buckets. |
| `POST /api/v1/downsample/ilp` | Reduce an InfluxDB Line Protocol payload into buckets. |

### `POST /api/v1/interpolate`

Reconstruct an irregular series onto a regular output grid using DSP's flagship
interpolation-on-read path (`splimes::auto_interpolate`, which selects
CPU / SIMD / parallel / GPU strategies internally).

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/interpolate \
  -H 'content-type: application/json' \
  -d '{
        "spline": "linear",
        "resolution": "seconds",
        "points": [
          { "timestamp": "1970-01-01T00:00:00Z", "value": 0.0 },
          { "timestamp": "1970-01-01T00:01:00Z", "value": 60.0 }
        ]
      }'
```

- `spline` — `linear` | `quadratic` | `cubic` (default) | `{"polynomial": {"degree": 3, "bounds_factor": 1.5}}`.
- `resolution` — output-grid step: `nanoseconds`..`years` (default `minutes`).
- `start` / `end` — optional inclusive range; default to the input's timestamp span.
- `points` — non-empty `{timestamp, value}` array.

The response carries the spline/resolution used, the input/output point counts,
and the interpolated `points`. Each output point carries a `kind` marking its
provenance — `raw` (the grid point coincides with an input observation),
`interpolated` (synthetic, within the observed span), or `extrapolated` (outside
it) — so a consumer never silently treats a synthetic value as an observed one.

### `POST /api/v1/interpolate/ilp`

Same engine, fed an **InfluxDB Line Protocol** payload — the wire format TSBS,
InfluxDB, and QuestDB speak. The payload is the `text/plain` body; the field,
precision, spline, and resolution are query parameters. The series range is the
data's own timestamp span.

```sh
printf 'cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n' | \
  curl -s -X POST \
    'http://127.0.0.1:8080/api/v1/interpolate/ilp?field=load&precision=s&spline=linear&resolution=seconds' \
    -H 'content-type: text/plain' --data-binary @-
```

- `field` (required) — the numeric field to project each record onto.
- `precision` — timestamp unit: `ns` (default) | `us` | `ms` | `s`.
- `spline` — `linear` | `quadratic` | `cubic` (default). *Polynomial is
  JSON-endpoint-only; it needs structured parameters.*
- `resolution` — as above (default `minutes`).

A malformed payload, an unknown token, fewer than two usable points, or a
zero-span series returns `400` with a `{"error": "..."}` body.

### `POST /api/v1/interpolate/point`

Single-instant lookup: evaluate the reconstructed signal at one timestamp. The
response labels the value **`raw`** (the instant coincides with an input
observation), **`interpolated`** (within the observed `[min, max]` span), or
**`extrapolated`** (outside it), so a caller never silently treats a synthetic
reconstruction as an observed value.

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/interpolate/point \
  -H 'content-type: application/json' \
  -d '{
        "spline": "linear",
        "instant": "1970-01-01T00:00:30Z",
        "points": [
          { "timestamp": "1970-01-01T00:00:00Z", "value": 0.0 },
          { "timestamp": "1970-01-01T00:01:00Z", "value": 60.0 }
        ]
      }'
# {"spline":"Linear","instant":"1970-01-01T00:00:30Z","input_points":2,"value":30.0,"kind":"interpolated"}
```

- `spline` — as the range endpoint (`linear` | `quadratic` | `cubic` (default) | polynomial).
- `instant` (required) — the RFC 3339 timestamp to evaluate at.
- `points` — non-empty `{timestamp, value}` array.

### `POST /api/v1/downsample`

The reduction counterpart of interpolation: instead of reconstructing a denser
series it groups the samples into fixed time buckets **aligned to the epoch grid**
for the chosen resolution (the same `Resolution::to_base` index the engine uses,
so a bucket's start is reproducible from the resolution alone) and reports a
per-bucket aggregate plus the bucket count. Only non-empty buckets are emitted —
gap-filling is the interpolation endpoint's job.

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/downsample \
  -H 'content-type: application/json' \
  -d '{
        "resolution": "minutes",
        "aggregations": ["min", "max", "avg"],
        "points": [
          { "timestamp": "1970-01-01T00:00:00Z", "value": 0.0 },
          { "timestamp": "1970-01-01T00:00:20Z", "value": 10.0 },
          { "timestamp": "1970-01-01T00:01:00Z", "value": 30.0 }
        ]
      }'
```

- `resolution` — bucket width: `nanoseconds`..`years` (default `minutes`).
- `aggregations` — any of `min` | `max` | `avg` | `sum` | `first` | `last`;
  defaults to `[min, max, avg]` when omitted or empty. Reductions are computed in
  `BigDecimal` (no float drift on the hot path). The bucket `count` is always
  reported regardless.
- `start` / `end` — optional inclusive window; default to the input span.
- `points` — non-empty `{timestamp, value}` array.

Each bucket in the response carries its grid-aligned `timestamp`, the `count` of
samples it held, and an `aggregations` map keyed by reduction name.

### `POST /api/v1/downsample/ilp`

The same reduction fed an **InfluxDB Line Protocol** payload. The payload is the
`text/plain` body; the field, precision, resolution, and aggregation list are
query parameters. The window is the data's own timestamp span.

```sh
printf 'cpu,host=a load=0 1000000020\ncpu,host=a load=20 1000000040\ncpu,host=a load=50 1000000080\n' | \
  curl -s -X POST \
    'http://127.0.0.1:8080/api/v1/downsample/ilp?field=load&precision=s&resolution=minutes&agg=min,max,avg' \
    -H 'content-type: text/plain' --data-binary @-
```

- `field` (required) — the numeric field to project each record onto.
- `precision` — timestamp unit: `ns` (default) | `us` | `ms` | `s`.
- `resolution` — as above (default `minutes`).
- `agg` — comma-separated reductions (e.g. `min,max,avg`); defaults to `min,max,avg`.

An unknown token, a malformed payload, or a field present in no record returns
`400` with a `{"error": "..."}` body.

## Numeric boundary

Wire values are JSON `f64`. DSP's logical/API numeric type is `BigDecimal` and
stays that way internally — request values are widened to `BigDecimal` before the
engine sees them. The `f64` on the wire is a transport convenience for this
slice, **not** a precision decision; schema-declared physical encodings
(Phase 4) replace it with explicit, lossless-by-default types.

## Metrics

`GET /metrics` renders the Prometheus text exposition format (v0.0.4):

```text
# HELP dsp_interpolate_requests_total Total interpolation requests received.
# TYPE dsp_interpolate_requests_total counter
dsp_interpolate_requests_total 1
dsp_interpolate_errors_total 0
dsp_interpolate_output_points_total 61
# HELP dsp_downsample_requests_total Total downsample requests received.
# TYPE dsp_downsample_requests_total counter
dsp_downsample_requests_total 1
dsp_downsample_errors_total 0
dsp_downsample_output_buckets_total 2
```

Both interpolation endpoints share the `dsp_interpolate_*` counters; both
downsample endpoints share the `dsp_downsample_*` counters.
