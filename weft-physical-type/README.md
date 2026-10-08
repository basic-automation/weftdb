# weft-physical-type

[![crates.io](https://img.shields.io/crates/v/weft-physical-type.svg)](https://crates.io/crates/weft-physical-type)
[![docs.rs](https://img.shields.io/docsrs/weft-physical-type)](https://docs.rs/weft-physical-type)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Declared **physical numeric encodings** for time-series values.

`BigDecimal` stays the logical, API-level value type. Each series declares the physical
encoding its values are stored in — `F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`
or `BigDecimalText` — together with an error bound.

The point of the crate is what happens when a value does not fit: **it is rejected, not
quietly rounded.** Downcasting is always explicit and always reports exactness, so there
is no silent `BigDecimal -> f64`. If you store money or calibrated instrument readings,
this is the difference between a store you can audit and one you cannot.

Also provides the typed columnar segment format (`.weftseg`) used on WeftDB's hot path,
including bit-packing, frame-of-reference and delta-of-delta coding, plus an opt-in
bit-sliced value layout (behind a feature, below).

```toml
[dependencies]
weft-physical-type = "0.1"
```

## Features

All features are off by default, are **not** covered by the 1.0 semver promise (their items
may change or disappear in any release), and are pending patent review. A default build
reads every frame written under the default configuration, that is with
`FrameOptions::transposed_max_overhead` unset (WeftDB's `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD`).
A frame written with that option set, which 0.1.0 did whenever it was set, needs
`bitsliced-codec`.

| Feature | Enables |
|---------|---------|
| `bitsliced-codec` | The opt-in bit-sliced (bit-plane-major) `ScaledI64` value codec, `scaled_transposed`. Without it the writer never selects the codec, and reading a segment that uses it fails with `WeftSegError::CodecNotEnabled` naming this feature. |
| `experimental-codecs` | Advisory codecs the `.weftseg` writer never emits: Gorilla-XOR, Chimp, Chimp128 and Elf for `f64` columns (plus the `best_f64_*` selector), and the Sprintz FIRE timestamp forecaster. Benchmark what-ifs, not a storage format. |

Part of [WeftDB](https://github.com/basic-automation/weftdb).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

The Chimp and Chimp128 codecs (behind `experimental-codecs`) are adapted from the authors'
reference implementation (Apache-2.0); see [THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES).
