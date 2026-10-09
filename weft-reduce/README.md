# weft-reduce

[![crates.io](https://img.shields.io/crates/v/weft-reduce.svg)](https://crates.io/crates/weft-reduce)
[![docs.rs](https://img.shields.io/docsrs/weft-reduce)](https://docs.rs/weft-reduce)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Vendor-neutral **time-series downsampling reductions**, in `BigDecimal`.

Reduces points onto an epoch-aligned bucket grid with `min`, `max`, `avg`, `sum`,
`first`, `last`, `count` and time-weighted averages (`twa_linear`, `twa_bucket_end`).
Reductions are **mergeable**, so partial results computed per segment can be combined
without re-reading the underlying values, and re-keyed to any coarser bucket width.

```toml
[dependencies]
weft-reduce = "0.1"
```

Part of [WeftDB](https://github.com/basic-automation/weftdb).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

The `DdSketch` quantile sketch is adapted from Datadog's DDSketch reference implementation
(sketches-java, Apache-2.0), and the integer `avg` division behind every reduction's `avg` is
adapted from bigdecimal-rs (MIT OR Apache-2.0, taken here under Apache-2.0). Those portions
stay under the Apache License 2.0 whichever option you choose;
[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES) carries the attribution and the license text.
