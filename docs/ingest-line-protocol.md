# Ingest-side InfluxDB Line Protocol reference

The `weft-line-protocol` crate parses and produces InfluxDB Line Protocol (ILP) — the
wire format that [InfluxDB](https://influxdb.com), [QuestDB](https://questdb.io) and the
[TSBS](https://github.com/timescale/tsbs) benchmark suite all speak.

Two of WeftDB's HTTP route families accept ILP bodies:
[**stateless compute**](#stateless-compute-interpolate--downsample) (interpolate,
downsample — `POST /api/v1/{interpolate,downsample}/ilp[/*]`) and
[**ingest**](#ingest-post-apiv1storageaspectilp)
(`POST /api/v1/storage/{aspect}/ilp`). Both use the same shared
[`weft-line-protocol`](../weft-line-protocol) parser and the same ILP dialect.

---

## Line record model

A single ILP line is parsed into a [`LineRecord`] :

| Field | Type | Notes |
|---|---|---|
| `measurement` | `String` | The table/series name (the first token before the first unescaped comma or space). |
| `tags` | `Vec<(String, String)>` | Key-value dimensions, in wire order. May be empty. Tag keys and values are always unescaped. |
| `fields` | `Vec<(String, FieldValue)>` | The typed measurements, in wire order. Always at least one. |
| `timestamp` | `Option<i64>` | The raw integer timestamp (caller-declared precision). `None` when omitted. |

A [`FieldValue`] is one of:

| ILP syntax | `FieldValue` variant |
|---|---|
| `1.5` | `Float(f64)` |
| `42i` | `Integer(i64)` |
| `42u` | `Unsigned(u64)` |
| `t` / `T` / `true` / `True` / `TRUE` | `Boolean(true)` |
| `f` / `F` / `false` / `False` / `FALSE` | `Boolean(false)` |
| `"hello"` | `Str(String)` |

A line without a field set, or a field without a value, is malformed.

Blank lines and lines beginning with `#` (after leading whitespace) are skipped.

---

## Building a record and writing it

Construct a [`LineRecord`] directly and call [`LineRecord::to_ilp()`] to produce the wire
text:

```rust
use weft_line_protocol::{FieldValue, LineRecord};

let record = LineRecord {
    measurement: "cpu".to_string(),
    tags: vec![("host".to_string(), "a".to_string())],
    fields: vec![("load".to_string(), FieldValue::Float(0.5))],
    timestamp: Some(1_000_000_000),
};
let text = record.to_ilp();
// text == "cpu,host=a load=0.5 1000000000"
```

The writer escapes measurement names, tag keys/values and field keys with backslashes
for the structural characters the parser treats as delimiters: `\\`, ` `, `,`, `=`.
String field values retain their double quotes with internal `\"` and `\\` escaped.

---

## Parsing text

### Parse into records

[`parse`] reads an entire document and returns `Vec<LineRecord>`:

```rust
use weft_line_protocol::parse;

let input = "cpu,host=a load=0.5 1000000000\ncpu,host=a load=0.8 2000000000\n";
let records = parse(input).expect("valid ILP");
```

Blank and comment lines are skipped. The first structurally malformed line returns
a [`ParseError`] carrying its 1-based line number and a human-readable message.

### Project into points

[`parse_points`] parses a document and projects a single named numeric field into a
sorted `Vec<Point>` — the primary on-ramp for TSBS-style single-series data:

```rust
use weft_line_protocol::{parse_points, TimestampPrecision};

let input = "weather temp=21.3 1000000000\nweather temp=22.1 2000000000\n";
let points = parse_points(input, "temp", TimestampPrecision::Nanoseconds)
    .expect("single-series ILP");
```

Every record contributing to the projection must belong to the same series (same
measurement and tag set); a second series is an
[`AmbiguousSeries`](ParseErrorKind::AmbiguousSeries) error. Records that lack the named
field or a timestamp are silently skipped.

---

## Escaping rules

The parser uses two escaping contexts:

| Context | Escaped characters | Example |
|---|---|---|
| Measurement, tag key/value, field key | `\\`, ` ` (space), `,`, `=` | `my\\ measurement,tag\\=key=tag\\,val` → measurement `my measurement`, tag key `tag=key`, tag value `tag,val` |
| Inside double-quoted string field value | `\\`, `\"` | `"a \\"quote\\""` → `a "quote"` |

### Carve-out: control characters

The writer escapes only `\\` and `"` inside double-quoted values (`escape_quoted`),
and the parser consumes the character after a backslash literally (`unescape`). That
cuts in two opposite directions:

- A **real control character** in a value — a raw newline, tab or carriage return — is
  emitted **verbatim**. The payload then stops being one record per line, and
  re-parsing cannot recover the record, because the parser splits input on newlines.
  Control characters are therefore **not representable** in this crate's ILP dialect.
- A **backslash you wrote yourself** is data, and it survives the trip. The value
  `line1<backslash>nline2` is written as `line1\\nline2` and reads back as the same
  `line1<backslash>nline2`; it is never reinterpreted as a newline.

```rust
use weft_line_protocol::{FieldValue, LineRecord, parse};

// The Rust literal "line1\\nline2" is the value line1<backslash>nline2.
let record = LineRecord {
    measurement: "m".to_string(),
    tags: vec![],
    fields: vec![("note".to_string(), FieldValue::Str("line1\\nline2".to_string()))],
    timestamp: Some(1_000_000_000),
};
let text = record.to_ilp(); // m note="line1\\nline2" 1000000000
let reparsed = parse(&text).expect("the backslash form round-trips");
assert_eq!(reparsed[0], record, "the backslash is preserved, not turned into a newline");
```

There is no representation for a value that actually contains a control character, so
`FieldValue::Str("line1\nline2".to_string())` — a real newline — cannot be emitted and
re-read. Strip or replace control characters before handing values to the writer.

### Carve-out: non-finite floats

The `f64` values `NaN`, `inf` and `-inf` are written by Rust's `Display` and re-parsed
successfully, so the **text** round-trips. However, `FieldValue` derives `PartialEq`,
and `f64::NAN != f64::NAN`, so a round-tripped record containing `NaN` is **not equal**
to the original record. This is a deliberate design choice: `FieldValue: Eq` is used as a
key (see `docs/stability.md`, issue #238), and adding a manual `PartialEq` that treats
`NaN` as equal to itself would be a breaking change.

```rust
use weft_line_protocol::parse;

let line = "m v=NaN 1";
let parsed = parse(&line).expect("NaN parses");
// parsed[0].to_ilp() == line  (text matches)
// parse(&parsed[0].to_ilp())[0] != parsed[0]  (records differ because NaN != NaN)
```

Additionally, [`parse_points`] **refuses** a non-finite named field (the ROB-7
behaviour): a line whose named field is `NaN`, `inf` or `-inf` returns a
[`NonFiniteValue`](ParseErrorKind::NonFiniteValue) error rather than being silently
skipped.

```rust
use weft_line_protocol::{parse_points, ParseErrorKind, TimestampPrecision};

let err = parse_points("weather temp=NaN 100\n", "temp", TimestampPrecision::Nanoseconds)
    .expect_err("non-finite values are refused, not dropped");
assert_eq!(err.kind, ParseErrorKind::NonFiniteValue);
```

---

## HTTP routes accepting ILP

All ILP-accepting routes use `POST`, take the ILP body as `text/plain`, and do not
require a specific `content-type` header (the body is read as UTF-8 raw text).

### Stateless compute: interpolate

| Route | Query parameters | Output format |
|---|---|---|
| `POST /api/v1/interpolate/ilp` | `field` (required), `precision`, `spline`, `interpolation`, `resolution` | JSON |
| `POST /api/v1/interpolate/ilp/csv` | same | CSV |
| `POST /api/v1/interpolate/ilp/arrow` | same | Arrow IPC stream |
| `POST /api/v1/interpolate/ilp/parquet` | same | Parquet |

**Query parameters:**

| Parameter | Values | Default | Notes |
|---|---|---|---|
| `field` | string | **(required)** | The numeric field key to project each record onto. |
| `precision` | `ns`, `us`, `ms`, `s` | `ns` | How the ILP integer timestamps are interpreted. Long forms (`nanoseconds`, `microseconds`, `millis`, `seconds`) also work. |
| `spline` | `linear`, `quadratic`, `cubic` | `cubic` | Spline method. Polynomial requires structured params (JSON endpoint only). |
| `interpolation` | same as `spline` | — | Alias for `spline` (for compatibility with other TSDB vocabularies). `spline` wins when both are supplied. |
| `resolution` | `seconds` … `years` or a width like `5s`, `15m` | `minutes` | Output grid step. |

The series span is the data's own timestamp range.

**Errors:** `400` for unknown precision/spline/resolution token, malformed payload,
fewer than two usable points, zero-span series, or an ambiguous series (TAG-0).

### Stateless compute: downsample

| Route | Query parameters | Output format |
|---|---|---|
| `POST /api/v1/downsample/ilp` | `field`, `precision`, `resolution`, `agg`, `fill` | JSON |
| `POST /api/v1/downsample/ilp/csv` | same | CSV |
| `POST /api/v1/downsample/ilp/arrow` | same | Arrow IPC stream |
| `POST /api/v1/downsample/ilp/parquet` | same | Parquet |

**Additional query parameters:**

| Parameter | Values | Default | Notes |
|---|---|---|---|
| `agg` | comma-separated: `min`, `max`, `avg`, `sum`, `first`, `last`, `p50`/`median`, `p90`, `p95`, `p99`, `sketch_p50`/`sketch_median`, `sketch_p90`, `sketch_p95`, `sketch_p99`, `twa`, `twa_linear`, `twa_bucket_end` | `min,max,avg` | One or more reduction methods. |
| `fill` | `null`, `prev`/`locf`, `linear`, `quadratic`, `cubic`, or a numeric constant | — | Gap-fill method for empty buckets. |

### Ingest: POST /api/v1/storage/{aspect}/ilp

| Route | Query parameters |
|---|---|
| `POST /api/v1/storage/{aspect}/ilp` | `measurement` (required), `field`, `precision`, `rows_per_page`, `require_sorted` |

**Query parameters:**

| Parameter | Values | Default | Notes |
|---|---|---|---|
| `measurement` | string | **(required)** | The ILP measurement whose records are sealed. Records of any other measurement in the payload are skipped. |
| `field` | string | **(required)** | The numeric field key to seal into the aspect. |
| `precision` | `ns`, `us`, `ms`, `s` | `ns` | Wire timestamp precision. |
| `rows_per_page` | positive integer | — | Optional page height for paged segments. |
| `require_sorted` | `true` / `false` | `false` | When true, rejects out-of-order batches with `400` instead of sealing them. |

The records of `measurement` carrying `field` must share one tag set, which names the
series the batch writes. No tags means the untagged series (ID 0). A payload with two
different tag sets for the same `measurement` + `field` combination is refused `400
ambiguous_series`.

**Errors (all `400`):** `invalid_tag` (reserved `__` key, repeated key, empty value,
more than 16 tags), `series_limit` (new series beyond `WEFT_MAX_SERIES_PER_ASPECT`),
`not_finite` (a `NaN`, `inf` or `-inf` field value — the same ROB-7 refusal as
`parse_points`), `ambiguous_series`, malformed payload.

---

## Timestamp handling

ILP integer timestamps carry no implicit unit. The caller declares the unit via
[`TimestampPrecision`] (or the HTTP query parameter `precision`).

| Token | Unit | Scale to nanoseconds |
|---|---|---|
| `ns` (default) | Nanoseconds | ×1 |
| `us` | Microseconds | ×1 000 |
| `ms` / `millis` | Milliseconds | ×1 000 000 |
| `s` / `sec` | Seconds | ×1 000 000 000 |

If scaling overflows `i64` the conversion returns `None` / the route answers `400`.

On the ingest path ([`POST /api/v1/storage/{aspect}/ilp`]), each absolute instant is
rescaled to the aspect's declared `TimeUnit`, so the wire precision and the stored
unit can differ.

Timestamps may be omitted from an ILP line; in that case `timestamp` is `None` and the
record is skipped by [`parse_points`] (and by the ingest projection).

---

## What is not supported

- **No authentication.** The crate is a format parser; authentication is the HTTP
  server's responsibility (see SEC-6 / `docs/security.md`).
- **No InfluxDB v2 line protocol extensions** (semicolon-separated fields, typed
  literals beyond the ones listed above, or the `\xNN` escape). WeftDB's parser
  accepts what TSBS and common exporters produce.
- **No batch or chunked parsing.** Each body is parsed as a whole document; streaming
  multi-gigabyte files is not supported by the parser.
- **No implicit series aggregation.** A payload carrying two tag sets for the same
  field is an error, not a merged stream (TAG-0).
- **Network or connection handling.** See the crate README: this is deliberately a
  *format* parser, not an InfluxDB client.

---

## See also

- [`weft-line-protocol` crate](../weft-line-protocol) — the parser library with its
  full API documentation.
- [`weft-client`](../weft-client) — the Rust HTTP client, which exposes ILP routes as
  typed methods.
- [`docs/reference.md`](reference.md) — the full server reference.