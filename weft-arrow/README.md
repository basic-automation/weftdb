# weft-arrow

[![crates.io](https://img.shields.io/crates/v/weft-arrow.svg)](https://crates.io/crates/weft-arrow)
[![docs.rs](https://img.shields.io/docsrs/weft-arrow)](https://docs.rs/weft-arrow)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Apache Arrow interchange** for WeftDB's typed columnar segments.

Converts a sealed [`weft-physical-type`](https://crates.io/crates/weft-physical-type)
segment to an Arrow `RecordBatch` and back, and serializes to Arrow IPC or Parquet bytes.

Arrow lives in this crate specifically so the heavy `arrow-*` dependency tree stays out
of the lean hot-path crates. Arrow here is an open in-memory **interchange** format — it
is not a storage backend and not a vendor connector.

```toml
[dependencies]
weft-arrow = "0.1"
```

Part of [WeftDB](https://github.com/basic-automation/weftdb).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
