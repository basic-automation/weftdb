# weftdb

[![crates.io](https://img.shields.io/crates/v/weftdb.svg)](https://crates.io/crates/weftdb)
[![docs.rs](https://img.shields.io/docsrs/weftdb)](https://docs.rs/weftdb)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Time-series database for irregularly sampled data.** Interpolation is a first-class
query, and every point a query returns is labelled `raw`, `interpolated` or `extrapolated`.

Most time-series stores ask you to choose a sampling grid on the way in and then punish you
for changing it. This crate stores what was observed, when it was observed, and how exact
that observation was — a query names the resolution and the spline it wants, and the answer
carries the provenance of each point, so a derived value never masquerades as a measured
one.

- `analyze_point` resolves one timestamp to one value, at the resolution you ask for.
- `analyze_range` streams a span at a resolution, without materialising the whole range.
- Provenance comes from [`splimes`](https://crates.io/crates/splimes), which carries the
  interpolation kernels; the label is part of the returned point, not a side channel.
- Subjects, aspects and datasets are addressable identifiers, so one store holds many
  sources without one schema per sensor.

```toml
[dependencies]
weftdb = "0.1"
splimes = "1"
bigdecimal = "0.4"
chrono = "0.4"
```

```rust,ignore
use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use splimes::{Resolution, Spline};
use weftdb::{Database, DatasetId, InputMeasurement};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let db = Database::new("reactor-1").await?;
let subject = db.observe_subject("coolant-loop").await?;
let aspect = db
    .track_aspect(&subject.id(), "temperature", &Resolution::Seconds, None)
    .await?;

// Observations arrive when the sensor decides to send them, not on a grid.
for (minute, value) in [(0, "20.5"), (5, "21.0"), (10, "21.5")] {
    let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, minute, 0).unwrap();
    let measurement = InputMeasurement::new(timestamp, BigDecimal::from_str(value)?);
    db.capture_measurement(&aspect.id(), &DatasetId::new(), &measurement).await?;
}

// What was it at 12:07:30, on a one-second grid, linearly interpolated?
let point = db
    .analyze_point(&aspect.id(), Utc.with_ymd_and_hms(2023, 1, 1, 12, 7, 30).unwrap(), &Resolution::Seconds, &Spline::Linear)
    .await?;
assert_eq!(point.value, BigDecimal::from_str("21.25").unwrap());
# Ok(())
# }
```

## Feature flags

| Feature | Default | What it turns on |
| --- | --- | --- |
| `fault-injection` | off | The crash-consistency test harness (`durable::fault` actions, the `durable::sim` power-cut filesystem). Without it `fault::hit` is a no-op and `WEFT_FAULT` is never read, so a production build cannot be made to abort. |
| `bitsliced-codec` | off | The opt-in bit-sliced (bit-plane-major) value codec. Outside the semver promise while it is under patent review. |

## Status

The library crates of this workspace are released together with the 1.0 cut and are not on
crates.io before it; until then, depend on the git checkout. The version on this branch is
in `[package].version` in [Cargo.toml](Cargo.toml), and
[RELEASING.md](https://github.com/basic-automation/weftdb/blob/main/RELEASING.md) says what
a release publishes.

Part of [WeftDB](https://github.com/basic-automation/weftdb) — the roadmap for what is
finished and what is planned is
[ROADMAP.md](https://github.com/basic-automation/weftdb/blob/main/ROADMAP.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
