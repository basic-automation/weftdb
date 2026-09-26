# weft-line-protocol

[![crates.io](https://img.shields.io/crates/v/weft-line-protocol.svg)](https://crates.io/crates/weft-line-protocol)
[![docs.rs](https://img.shields.io/docsrs/weft-line-protocol)](https://docs.rs/weft-line-protocol)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A **format parser** for InfluxDB Line Protocol (ILP). Text in, neutral records out.

This is deliberately *not* an InfluxDB client: no network, no HTTP, no vendor SDK, no
connection handling. ILP is treated purely as a wire format, so you can accept data from
anything that already speaks it — Telegraf, sensors, exporters — without taking on a
vendor dependency.

Parses measurements, tag sets, field sets and timestamps into neutral records, or
directly into [`splimes`](https://crates.io/crates/splimes) points.

```toml
[dependencies]
weft-line-protocol = "0.1"
```

Part of [WeftDB](https://github.com/basic-automation/weftdb).

## License

MIT — see [LICENSE](LICENSE).
