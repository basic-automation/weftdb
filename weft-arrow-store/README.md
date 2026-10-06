# weft-arrow-store

[![crates.io](https://img.shields.io/crates/v/weft-arrow-store.svg)](https://crates.io/crates/weft-arrow-store)
[![docs.rs](https://img.shields.io/docsrs/weft-arrow-store)](https://docs.rs/weft-arrow-store)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Read a stored [WeftDB](https://crates.io/crates/weftdb) aspect range straight into an
Apache Arrow `RecordBatch`.

This is the bridge crate: it is the one place allowed to depend on both `weftdb` and
[`weft-arrow`](https://crates.io/crates/weft-arrow), which keeps either heavy dependency
from reaching the lean hot-path core. Depend on it when you want stored data in Arrow;
depend on `weft-arrow` alone when you only need segment conversion.

```toml
[dependencies]
weft-arrow-store = "0.1"
```

Part of [WeftDB](https://github.com/basic-automation/weftdb).

## License

MIT — see [LICENSE](LICENSE).
