# weft-orchestration

[![crates.io](https://img.shields.io/crates/v/weft-orchestration.svg)](https://crates.io/crates/weft-orchestration)
[![docs.rs](https://img.shields.io/docsrs/weft-orchestration)](https://docs.rs/weft-orchestration)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The **analytics pipeline** over stored [WeftDB](https://crates.io/crates/weftdb) aspects.

Chains batching → pattern extraction → event detection → correlation → signal generation
behind one fluent `Pipeline` builder, with built-in detectors and parallel execution.
Each stage is incremental and can also be driven directly (`prepare_data`,
`extract_patterns`, `detect_events`, `correlate_events`, `generate_signals`).

```rust,ignore
use weft_orchestration::Pipeline;
use weftdb::Database;
use splimes::Spline;

let database = Database::existing("PlantTelemetry").await?;

let mut pipeline = Pipeline::builder(database.clone(), aspect_id)
    .spline_method(Spline::Linear)
    .batch_size(24)
    .with_monthly_increase_detector(0.05)
    .with_peak_detector("Pressure Peaks")
    .build()
    .await?;

pipeline.run().await?;
```

```toml
[dependencies]
weft-orchestration = "0.1"
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
