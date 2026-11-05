# weft-client

An async Rust client for [WeftDB](https://github.com/basic-automation/weftdb)'s
`weft-server` HTTP API: one method per route, named for what it does.

```rust,no_run
use weft_client::{AspectDeclaration, Client, Interpolate, Spline, WriteOptions};

# async fn demo() -> weft_client::Result<()> {
let client = Client::builder("http://127.0.0.1:8080").api_key("weft_…").build()?;

// Lossless storage: values go in as decimal text, BigDecimal or integers, and come back as BigDecimal.
client.declare_aspect(&AspectDeclaration::new("price", "scaled_i64", "millis").scale(2)).await?;
client.write_points("price", [(1_000, "1.25"), (2_000, "2.50")], &WriteOptions::idempotent("batch-1")).await?;
let at = client.at("price", 2_000).await?;
assert_eq!(at.value.unwrap().to_string(), "2.50");

// Compute: f64 in, a reconstructed grid out, every point marked raw, interpolated or extrapolated.
let points = [(chrono::Utc::now(), 1.0)];
let grid = client.interpolate(&Interpolate::new(points).spline(Spline::Linear)).await?;
# let _ = grid; Ok(()) }
```

- **Every route**: compute (interpolate, downsample, their Line Protocol forms, as JSON,
  CSV, Arrow or Parquet), catalog, ingest (JSON points, Line Protocol, CSV, Parquet, with
  `Idempotency-Key`), stored reads (pages, cursor-following `Stream`s, single instants,
  columnar ranges, stored downsample), maintenance, backup and the benchmark runs.
- **No floats in the store by accident.** `f64` does not implement `StoredValue`, so
  `write_points("p", [(1, 1.25_f64)], …)` does not compile.
- **One error type** carrying the server's whole envelope: `code`, `details`, `limit`,
  `committed` and the `request_id` to quote to an operator. A read refused
  `503 overloaded` is retried after `Retry-After`; a write never is.
- **Features**: `rustls` for HTTPS (ring provider; the platform's roots, or
  `root_certificates_pem` for a private CA), `arrow` to decode Arrow IPC and Parquet answers
  into `RecordBatch`es.

The server's API is described in
[`docs/reference.md`](https://github.com/basic-automation/weftdb/blob/main/docs/reference.md).

Licensed under either of MIT or Apache-2.0, at your option.
