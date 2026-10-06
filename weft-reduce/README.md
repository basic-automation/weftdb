# weft-reduce

[![crates.io](https://img.shields.io/crates/v/weft-reduce.svg)](https://crates.io/crates/weft-reduce)
[![docs.rs](https://img.shields.io/docsrs/weft-reduce)](https://docs.rs/weft-reduce)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

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

MIT — see [LICENSE](LICENSE).
