# weft-physical-type

[![crates.io](https://img.shields.io/crates/v/weft-physical-type.svg)](https://crates.io/crates/weft-physical-type)
[![docs.rs](https://img.shields.io/docsrs/weft-physical-type)](https://docs.rs/weft-physical-type)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Declared **physical numeric encodings** for time-series values.

`BigDecimal` stays the logical, API-level value type. Each series declares the physical
encoding its values are stored in — `F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`
or `BigDecimalText` — together with an error bound.

The point of the crate is what happens when a value does not fit: **it is rejected, not
quietly rounded.** Downcasting is always explicit and always reports exactness, so there
is no silent `BigDecimal -> f64`. If you store money or calibrated instrument readings,
this is the difference between a store you can audit and one you cannot.

Also provides the typed columnar segment format (`.weftseg`) used on WeftDB's hot path,
including bit-packing, delta-of-delta timestamp coding and a transposed value layout.

```toml
[dependencies]
weft-physical-type = "0.1"
```

Part of [WeftDB](https://github.com/basic-automation/weftdb).

## License

MIT — see [LICENSE](LICENSE).
