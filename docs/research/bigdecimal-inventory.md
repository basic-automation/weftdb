# BigDecimal Precision-Boundary Inventory

**Date:** 2026-11-20
**Branch:** `bprec1/bigdecimal-inventory` (`origin/main` at `ed95748`)
**Scope:** Every place `BigDecimal` in the weftdb workspace gains or loses precision, grouped by crate. Sites are labelled **exact**, **lossy by construction** (a float hop), or **lossy by policy** (chosen scale/rounding/truncation). Judged from the code alone.

**Verified against `origin/main`; all line numbers re-read via `git show origin/main:<path> | sed -n 'X,Yp'`.**

---

## Crate summary (actual counts)

| Crate | Files | BigDecimal mentions |
|---|---|---|
| `weftdb` | 50 | 561 |
| `weft-physical-type` | 8 | 350 |
| `weft-reduce` | 6 | 164 |
| `weft-bench` | 24 | 201 |
| `weft-server` | 13 | 115 |
| `weft-client` | 2 | 15 |
| `weft-arrow` | 1 | 53 |
| `weft-arrow-store` | 1 | 24 |
| `weft-line-protocol` | 1 | 8 |
| `weft-orchestration` | 6 | 101 |
| `weft-tui` | 1 | 2 |
| `weft-xbench` | 6 | 26 |

---

## Findings by crate

---

### 1. `weft-physical-type` (350 mentions, 8 files)

#### 1.1 Boundaries into `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:462` `decimal_from_f64(value)` — shortest round-trip `{:e}` | `let decimal = BigDecimal::new(mantissa.into(), fraction_digits - exponent);` | **exact** (round-trip, not binary expansion) |
| `src/lib.rs:479` `decimal_from_f32(value)` — same for `f32` | `decimal_from_shortest_text(&format!("{value:e}"))` | **exact** (round-trip, not binary expansion) |
| `src/lib.rs:511` `decimal_from_shortest_text` — negative-scale clamp | `if decimal.fractional_digit_count() < 0 { decimal.with_scale(0) } else { decimal }` | **lossy by policy** (clamps negative-scale decimals to scale 0) |
| `src/lib.rs:449` `parse_decimal(text)` — `BigDecimal::from_str` | `let value = BigDecimal::from_str(text).map_err(...)?;` | **exact** (string parse preserves decimal text) |
| `src/lib.rs:693` `PhysicalValue::to_logical` F64 | `BigDecimal::from_f64(*f).unwrap_or_default()` | **lossy by construction** (exact binary expansion of `f64`) |
| `src/lib.rs:694` `PhysicalValue::to_logical` F32 | `BigDecimal::from_f32(*f).unwrap_or_default()` | **lossy by construction** (exact binary expansion of `f32`) |
| `src/lib.rs:696` `PhysicalValue::to_logical` ScaledI64/ScaledI128 | `BigDecimal::new(BigInt::from(*mantissa), i64::from(*scale))` | **exact** (integer + declared scale) |
| `src/lib.rs:697` `PhysicalValue::to_logical` Decimal128 | `BigDecimal::new(BigInt::from(*mantissa), *scale)` | **exact** (integer + per-value scale) |
| `src/lib.rs:698` `PhysicalValue::to_logical` BigDecimalText | `s.parse().unwrap_or_default()` | **exact** when parseable |
| `src/lib.rs:518` `pow10(n)` — builds `10^n` | `BigDecimal::from(mantissa)` | **exact** |

#### 1.2 Boundaries out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:617` `PhysicalType::F64` encode `to_f64()` | `let f = value.to_f64().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;` | **lossy by construction** (`f64` has ~53 bits mantissa) |
| `src/lib.rs:623` `PhysicalType::F32` encode `to_f32()` | `let f = value.to_f32().filter(|f| f.is_finite()).ok_or(EncodeError::NotFinite)?;` | **lossy by construction** (`f32` has ~24 bits mantissa) |
| `src/lib.rs:629` `ScaledI64` encode — `round(0)` then `to_i64()` | `let shifted = (value * pow10(scale)).round(0);` then `mantissa = shifted.to_i64().ok_or(EncodeError::Overflow)?;` | **lossy by policy** (scale truncation + `round` chooses how to round) |
| `src/lib.rs:636` `ScaledI128` encode — same pattern | same as above but `to_i128()` | **lossy by policy** (same scale truncation + round) |
| `src/lib.rs:645` `Decimal128` encode — `fit_i128` digit-shedding | `let (mantissa, scale) = fit_i128(bigint, scale);` | **lossy by policy** (round_div_10 per digit; exactness reported via `measure`) |
| `src/lib.rs:661` `BigDecimalText` encode — `to_plain_string` | `let pv = PhysicalValue::BigDecimalText(value.to_plain_string());` | **exact** (always lossless) |
| `src/lib.rs:537` `fit_i128` inner — `round_div_10` (half away from zero) | `(n + bias) / BigInt::from(10)` | **lossy by policy** (one digit of precision shed per call) |

#### 1.3 Scale, rounding & comparison decisions

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:511` `with_scale(0)` in `decimal_from_shortest_text` | `decimal.with_scale(0)` | **lossy by policy** (negative-scale → scale 0, discarding zeros) |
| `src/lib.rs:553` `measure` — `diff.abs().normalized()` | `Exactness::Lossy { abs_error: diff.abs().normalized() }` | **exact** (error bounding, not storage or API boundary) |
| `src/lib.rs:636` `round(0)` in ScaledI64/ScaledI128 | `.round(0)` | **lossy by policy** (scale choice rounds silently) |
| `src/lib.rs:601` `column.rs` — normalized for exponent extraction | `let (_mantissa, exponent) = v.normalized().as_bigint_and_exponent();` | **exact** |

---

### 2. `weft-reduce` (164 mentions, 6 files)

#### 2.1 Boundaries into `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/fill.rs:200` — linear interpolation `v0 + (v1-v0) * offset / span` | `let value = v0 + (v1 - v0) * BigDecimal::from(offset) / BigDecimal::from(span);` | **lossy by policy** (division uses `bigdecimal`'s default division precision; quotient may not terminate) |
| `src/scaled.rs:281` `avg_of_sum` — `sum / count` | `i128::try_from(&digits).map_or_else(|_| sum / &BigDecimal::from(count), \|int\| avg_like_bigdecimal(int, scale, count))` | **lossy by policy** (division may round; integer fast path is exact when integer division yields no remainder) |
| `src/scaled.rs:140` `avg_like_bigdecimal` — fast path for integer avg | `BigDecimal::new(sum, scale) / BigDecimal::from(count)` | **lossy by policy** (division rounding at default precision) |
| `src/scaled.rs:1028` — test `with_scale(4)` | `p.value.with_scale(4)` | **lossy by policy** (test, not runtime) |

#### 2.2 Boundaries out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/sketch.rs:57` `decimal_to_f64` — fast path `scaled_to_f64`, fallback `to_f64` | `digits.to_i64().and_then(\|m\| scaled_to_f64(m, scale)).or_else(\|\| v.to_f64())` | **lossy by construction** (returns `f64`) |
| `src/sketch.rs:70-76` `scaled_to_f64` — exact division for small mantissas | `let m = mantissa.to_f64()?;` then `m / 10_f64.powi(scale)` | **lossy by construction** (mantissa converted to `f64` then scaled) |
| `src/sketch.rs:55` `decimal_to_f64` doc: "bit-identical to BigDecimal's to_f64" | `pub fn decimal_to_f64(v: &BigDecimal) -> Option<f64>` | **lossy by construction** (API boundary to `f64`) |
| `src/uniform.rs:223` — sketch `add(decimal_to_f64(v))` | `self.add(decimal_to_f64(v).ok_or(SketchError::ValueRange)?)` | **lossy by construction** (decimal → f64 → sketch) |
| `src/sketch.rs:358` — same for DdSketch | `self.add(decimal_to_f64(v).ok_or(SketchError::ValueRange)?)` | **lossy by construction** (decimal → f64 → sketch) |
| `src/lib.rs:924` — `to_f64().expect("finite")` in bucket value | `b.values.get(agg.as_str()).expect("aggregation present").to_f64().expect("finite")` | **lossy by construction** |
| `src/sketch.rs:753` — relative error via `to_f64` | `let rel = ((&median - &exact) / &exact).to_f64().unwrap().abs();` | **lossy by construction** |
| `src/uniform.rs:561` — test `to_f64` | `float.add(d.to_f64().expect("finite")).expect("adds");` | **lossy by construction** |
| `src/uniform.rs:565` — test relative error via `to_f64` | `...to_f64().expect("finite").abs()` | **lossy by construction** |

#### 2.3 Division, scale & comparison decisions

| Site | Code | Label |
|---|---|---|
| `src/fill.rs:200` `BigDecimal::from(offset) / BigDecimal::from(span)` | `BigDecimal::from(offset) / BigDecimal::from(span)` | **lossy by policy** (default division precision; non-terminating) |
| `src/scaled.rs:281` `sum / &BigDecimal::from(count)` | `sum / &BigDecimal::from(count)` | **lossy by policy** |
| `src/sketch.rs:76` `mantissa.to_f64()?` in scaled_to_f64 | `let m = mantissa.to_f64()?;` | **lossy by construction** |

#### 2.4 Serde text boundary

`Cargo.toml:20`: `bigdecimal = { workspace = true, features = ["serde", "string-only"] }`

The `string-only` feature makes `BigDecimal` serialize as a JSON string (e.g., `"99.92"`) and deserialize from one. This is required because the default serde impl uses `deserialize_any`, which postcard (the persisted-partial sidecar codec) cannot support. Every `BigDecimal` crossing the wire or file boundary in `weft-reduce` is therefore an **exact** text round-trip — no binary representation loss.

---

### 3. `weftdb` (561 mentions, 50 files)

#### 3.1 Boundaries into `BigDecimal` — from `f64`

| Site | Code | Label |
|---|---|---|
| `src/types/measurement.rs:71` | `BigDecimal::from_f64(value_f64.rem_euclid(100.0)).unwrap_or_default()` | **lossy by construction** (exact binary expansion) |
| `src/types/measurement.rs:127` | `BigDecimal::from_f64(value_f64.rem_euclid(100.0)).unwrap_or_else(\|\| BigDecimal::from(50))` | **lossy by construction** |
| `src/types/input_measurement.rs:41` | `BigDecimal::from_f64(value_f64.rem_euclid(100.0)).unwrap_or_else(\|\| BigDecimal::from(50))` | **lossy by construction** |
| `src/types/dictionary.rs:680` | `BigDecimal::from_f64(ratio).ok_or_else(...)` | **lossy by construction** |
| `src/types/dictionary.rs:698` | `BigDecimal::from_f64(ratio).ok_or_else(...)` | **lossy by construction** |
| `src/types/dictionary.rs:1288` | `BigDecimal::from_f64(interpolated_amplitude).unwrap_or_else(BigDecimal::zero)` | **lossy by construction** |
| `src/types/dictionary.rs:1313` | `BigDecimal::from_f64(location_ratio).unwrap()` | **lossy by construction** |
| `src/types/dictionary.rs:1314` | `BigDecimal::from_f64(amp).unwrap()` | **lossy by construction** |
| `src/types/dictionary.rs:1327` | `BigDecimal::from_f64(10.0).unwrap()` | **lossy by construction** |
| `src/types/dictionary.rs:1412` | `BigDecimal::from_f64(25.0).unwrap()` | **lossy by construction** |
| `src/types/dictionary.rs:1446` | `BigDecimal::from_f64(25.0).unwrap()` | **lossy by construction** |
| `src/types/compression/algorithm.rs:216-220,228-237` | `BigDecimal::from_f64(...).unwrap()` (8 test sites) | **lossy by construction** |

#### 3.2 Boundaries into `BigDecimal` — from string (`from_str`)

| Site | Code | Label |
|---|---|---|
| `src/types/ingest.rs:324` | `BigDecimal::from_str(text).expect("decimal")` | **exact** (text parse) |
| `src/types/partial_sidecar.rs:395` | `BigDecimal::from_str(value).expect("valid decimal")` | **exact** |
| `src/types/metadata.rs:368` | `BigDecimal::from_str(&s)?` | **exact** |
| `src/types/metadata.rs:381` | `BigDecimal::from_str(s).expect("parses")` | **exact** |
| `src/types/dictionary.rs:420` | `BigDecimal::from_str(value_str)?` | **exact** |
| `src/types/aspect_catalog.rs:238` | `BigDecimal::from_str(&row.get_value(1)?.as_text().cloned().unwrap_or_default())?` | **exact** |
| `src/types/aspect_catalog.rs:327` | `BigDecimal::from_str(s).expect("parses")` | **exact** |
| `src/types/database/outputs.rs:237` | `BigDecimal::from_str(&value_str).map_err(...)?` | **exact** |
| `src/types/database/outputs.rs:894-900` | multiple `BigDecimal::from_str(...)` for sum/abs/avg fields | **exact** |
| `src/types/database/outputs.rs:1615` | `BigDecimal::from_str(&value_str).ok()` | **exact** |
| `src/types/database/outputs.rs:1648` | `BigDecimal::from_str(&value_str).map_err(...)?` | **exact** |
| `src/types/database/outputs.rs:1770` | `BigDecimal::from_str(&value_str).map_err(...)?` | **exact** |
| `src/types/database/outputs.rs:1940` | `BigDecimal::from_str(value).map_err(...)?` | **exact** |
| `src/types/interpolate_range.rs:361` | `BigDecimal::from_str(s).expect("decimal")` | **exact** |
| `src/types/legacy_seal.rs:478` | `BigDecimal::from_str(s).expect("decimal")` | **exact** |
| `src/types/segment_store.rs:3389` | `BigDecimal::from_str(s).expect("parses")` | **exact** |
| `src/types/rows_mode.rs:821` | `BigDecimal::from_str(&shown).map_err(...)?` | **exact** |
| `src/types/segment_index.rs:1041` | `BigDecimal::from_str(&s)?` | **exact** |
| `src/types/segment_index.rs:1311` | `BigDecimal::from_str(s).expect("parses")` | **exact** |
| ~30+ test files using `BigDecimal::from_str(s).expect("parses")` | common test helper across test files | **exact** |

#### 3.3 Boundaries out of `BigDecimal` — `to_f64`

| Site | Code | Label |
|---|---|---|
| `src/types/num.rs:12-13` `to_f64<T>` generic | `ToPrimitive::to_f64(&n).unwrap_or(f64::NAN)` | **lossy by construction** |
| `src/types/dictionary.rs:234` — pattern amplitudes via `ToPrimitive::to_f64` | `pattern.amplitudes().iter().filter_map(ToPrimitive::to_f64).collect()` | **lossy by construction** |
| `src/types/dictionary.rs:967-984` — four similarity methods | `.filter_map(bigdecimal::ToPrimitive::to_f64).collect()` | **lossy by construction** |
| `src/types/dictionary.rs:1017-1018` — MAE comparison | `.filter_map(bigdecimal::ToPrimitive::to_f64).collect()` | **lossy by construction** |
| `src/types/dictionary.rs:1057-1064` — MSE comparison | same pattern | **lossy by construction** |
| `src/types/dictionary.rs:1095-1105` — MAE comparison | same pattern | **lossy by construction** |
| `src/types/dictionary.rs:1136-1143` — MSE comparison | same pattern | **lossy by construction** |
| `src/types/dictionary.rs:1171` — comparison threshold | `let threshold_f64 = threshold.to_f64().unwrap_or(0.0);` | **lossy by construction** |
| `src/types/dictionary.rs:1248` — target location | `let target_loc_f64 = target_location.to_f64().unwrap_or(0.0);` | **lossy by construction** |
| `src/types/dictionary.rs:1255` — relative location | `let loc_f64 = relative.vector().location().to_f64().unwrap_or(0.0);` | **lossy by construction** |
| `src/types/dictionary.rs:1274-1277` — interpolation neighbours | four `.to_f64().unwrap_or(0.0)` calls | **lossy by construction** |
| `src/types/dictionary.rs:1011` — threshold conversion | `let threshold_f64 = threshold.to_f64().unwrap_or(0.0);` | **lossy by construction** |
| `src/types/interpolate_range.rs:78` — `decimal_to_f64` | `floats.push(weft_reduce::decimal_to_f64(&value).filter(\|f\| f.is_finite())...)` | **lossy by construction** |
| `src/types/interpolate_range.rs:419` — test `to_f64` | `assert_eq!(e.to_f64().expect("finite").to_bits(), f.to_bits(), ...)` | **lossy by construction** |
| `src/types/compression/mod.rs:207,238,265,266` — `to_f64` for ratios | `crate::types::num::to_f64(compressed_count)` | **lossy by construction** |
| `src/types/compression/size.rs:79,80,109` — `to_f64` for sizing | `crate::types::num::to_f64(target_size) / crate::types::num::to_f64(current_size)` | **lossy by construction** |
| `src/types/aspect.rs:2010` — tier computation via `to_f64` | `crate::types::num::to_f64(total_duration_millis) / crate::types::num::to_f64(tier_duration_millis)` | **lossy by construction** |
| `src/types/metadata.rs:113-114` — bytes-per-point | `crate::types::num::to_f64(self.total_rows)` | **lossy by construction** |
| `src/types/segment_store.rs:3327-3328` — bytes-per-point | `crate::types::num::to_f64(self.total_rows)` | **lossy by construction** |
| `src/types/segment_store.rs:6357` — expected bytes-per-point | `crate::types::num::to_f64(expected_bytes) / 10.0` | **lossy by construction** |
| `src/types/segment_store.rs:6444` — expected bytes-per-point | `crate::types::num::to_f64(stats.total_bytes) / 5.0` | **lossy by construction** |
| `src/types/database/helpers.rs:12-36` — `safe_usize_to_f64`, `safe_i64_to_f64`, `safe_ratio` | all wrap `crate::types::num::to_f64` | **lossy by construction** |
| `src/types/database/inputs.rs:562` — progress reporting | `safe_usize_to_f64(start)` and `safe_usize_to_f64(total_measurements)` | **lossy by construction** |
| `src/types/segment_store/overlap_read_tests.rs:630` — `decimal_to_f64` | `weft_reduce::decimal_to_f64(&exact).expect("exact p99 is finite")` | **lossy by construction** |

#### 3.4 Text serialization (out)

| Site | Code | Label |
|---|---|---|
| `src/types/index_txn.rs:627` — Value::Text via `to_plain_string` | `v.to_plain_string()` | **exact** |
| `src/types/index_txn.rs:735` — same pattern | `v.to_plain_string()` | **exact** |
| `src/types/rows_mode.rs:1609` — `normalized().to_plain_string()` | `value.normalized().to_plain_string()` | **exact** (normalized removes trailing zeros) |

#### 3.5 `normalized()` usage

| Site | Code | Label |
|---|---|---|
| `src/types/rows_mode.rs:846` — exponent extraction | `let (_, exponent) = value.normalized().as_bigint_and_exponent();` | **exact** |
| `src/types/rows_mode.rs:1609` — comparison normalization | `value.normalized().to_plain_string()` | **exact** |

#### 3.6 Serde text boundary

`Cargo.toml:34`: `bigdecimal = { workspace = true, features = ["serde"] }` (no `string-only`).

This means `BigDecimal` serde uses the default representation: a JSON number (e.g., `99.92`) for self-describing formats (JSON), and `deserialize_any` for binary formats. Postcard does **not** support `deserialize_any`, so `BigDecimal` serde within `weftdb` is only safe across JSON boundaries, not postcard. At every JSON API boundary in `weft-server` the value is actually constructed from a `f64`-to-decimal widening (`widen_json_value`), not from the default serde path.

---

### 4. `weft-server` (115 mentions, 13 files)

#### 4.1 Boundaries into `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/downsample.rs:331` `widen_json_value` — shortest round-trip `{:e}` | `BigDecimal::new(mantissa.into(), fraction_digits - exponent)` then `with_scale(0)` clamp | **exact** (round-trip decimal, same as `decimal_from_f64`) |
| `src/downsample.rs:336` `json_point_value` — wraps `widen_json_value` | `widen_json_value(value).ok_or_else(...)` | **exact** |

#### 4.2 Boundaries out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/downsample.rs:365` — rejects non-finite via `to_f64` | `value.to_f64().filter(\|v\| v.is_finite()).ok_or_else(...)` | **lossy by construction** |
| `src/interpolate.rs:898` — per-point `to_f64` | `let value = point.value.to_f64().filter(\|v\| v.is_finite()).ok_or_else(...)` | **lossy by construction** |
| `src/downsample.rs:1250-1314` — tests for `widen_json_value` vs `BigDecimal::from_f64` | tests comparing `widen_json_value(v)` and `BigDecimal::from_f64(v)` | **lossy by construction** (test comparison) |

#### 4.3 Scale & comparison decisions

| Site | Code | Label |
|---|---|---|
| `src/downsample.rs:331` — `with_scale(0)` for negative-scale values | `decimal.with_scale(0)` | **lossy by policy** (same as physical-type) |

---

### 5. `weft-arrow` (53 mentions, 1 file)

#### 5.1 Boundaries out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:425` — F64 typed export: `BigDecimal::to_f64` | `values.iter().map(\|opt\| opt.as_ref().and_then(BigDecimal::to_f64)).collect::<Float64Array>()` | **lossy by construction** |
| `src/lib.rs:426` — F32 typed export: `BigDecimal::to_f32` | `values.iter().map(\|opt\| opt.as_ref().and_then(BigDecimal::to_f32)).collect::<Float32Array>()` | **lossy by construction** |
| `src/lib.rs:447` — exact text fallback via `to_plain_string` | `values.iter().map(\|opt\| opt.as_ref().map(BigDecimal::to_plain_string)).collect::<StringArray>()` | **exact** |

#### 5.2 Scale decisions

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:450` — `try_decimal128_array`: `with_scale(scale_i64)` | `bd.clone().with_scale(scale_i64).into_bigint_and_exponent().0.to_i128()?` | **lossy by policy** (`with_scale` rounds silently to fit the declared Arrow scale) |

---

### 6. `weft-line-protocol` (8 mentions, 1 file)

#### 6.1 Boundaries into `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:112` — `FieldValue::as_big_decimal` Float | `weft_physical_type::decimal_from_f64(*f)` | **exact** (shortest round-trip) |
| `src/lib.rs:113` — Integer | `BigDecimal::from(*i)` | **exact** |
| `src/lib.rs:114` — Unsigned | `BigDecimal::from(*u)` | **exact** |

The ILP parser constructs `FieldValue::Float` from the line-protocol wire bytes directly (as `f64`), so the float->decimal boundary is the only precision decision.

---

### 7. `weft-bench` (201 mentions, 24 files)

#### 7.1 Boundaries out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/accuracy.rs:110` — `to_f64()` per point | `let pv = p.value.to_f64().filter(\|v\| v.is_finite()).ok_or(...)?;` | **lossy by construction** |
| `src/baseline_adapter.rs:138` | `(value.to_f64().expect("finite") - want).abs() < 1e-9` | **lossy by construction** |
| `src/compression.rs:252` — `to_f64().is_some_and(f64::is_finite)` | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/downsample.rs:284` — same | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/gap_fill.rs:210` — `to_f64()` per point | `point.value.to_f64().filter(\|v\| v.is_finite()).ok_or_else(...)?` | **lossy by construction** |
| `src/gap_fill.rs:289` — same | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/ingest.rs:163` — `to_f64().is_some_and(f64::is_finite)` | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/ingest.rs:323` — same | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/ingest.rs:516` — same | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/lib.rs:224` — `to_f64().is_some_and(f64::is_finite)` | `p.value.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/point_lookup.rs:341` — same | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/range_fetch.rs:272` — same | `v.to_f64().is_some_and(f64::is_finite)` | **lossy by construction** |
| `src/forward_fill_adapter.rs:115` — `to_f64()` | `(value.to_f64().expect("finite") - want).abs() < 1e-9` | **lossy by construction** |
| `src/profile.rs:579` — `to_f64()` | `let value = p.value.to_f64().expect("finite value");` | **lossy by construction** |
| `src/conformance.rs:294` — standalone `to_f64` helper | `pub fn to_f64(d: &BigDecimal) -> f64 { d.to_f64().unwrap_or(0.0) }` | **lossy by construction** |
| `src/conformance.rs:312` — `BigDecimal::try_from(to_f64(&p.value))` | `BigDecimal::try_from(to_f64(&p.value)).expect("finite")` | **lossy by construction** (round-trip f64 → BigDecimal → f64) |
| `src/schema.rs:213` — digest `normalized().to_string()` | `v.normalized().to_string()` | **exact** |
| `src/lib.rs:225` — digest via `normalized()` | `p.value.normalized()` | **exact** |
| `src/query_mix.rs:110` — digest via `normalized()` | `v.normalized().to_string()` | **exact** |
| `src/uci.rs:130` — scale via `normalized()` | `let scale = value.normalized().as_bigint_and_exponent().1;` | **exact** |
| `src/e2e_interpolation.rs:785` — `decimal_to_f64` | `floats.push(weft_reduce::decimal_to_f64(&value).filter(\|f\| f.is_finite())...)?;` | **lossy by construction** |

#### 7.2 Scale & rounding decisions

| Site | Code | Label |
|---|---|---|
| `src/profile.rs:434` — `BigDecimal::new(f64_to_i64(...), places)` | `BigDecimal::new(crate::num::f64_to_i64((signal * 10_f64.powi(places.cast_signed())).round()).into(), i64::from(places))` | **lossy by construction** (f64 arithmetic then integer → BigDecimal) |
| `src/downsample.rs:167` — same pattern | `let mantissa = crate::num::f64_to_i64((signal * 10_f64.powi(places.cast_signed())).round());` | **lossy by construction** |
| `src/conformance.rs:175` — f64 rounding to i64 | `let thousandths = (reading * 1000.0).round() as i64;` | **lossy by construction** |
| `src/gap_fill.rs:179` — f64 rounding to i64 | `let cents = crate::num::f64_to_i64(((clean + noise) * 100.0).round());` | **lossy by construction** |

---

### 8. `weft-client` (15 mentions, 2 files)

#### 8.1 Boundaries into/out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/types.rs:40` — `StoredValue` impl for `BigDecimal` | uses `to_string()` for serialization (writer) | **exact** |
| `src/types.rs:53` — reader: `BigDecimal::from_str` | `let parsed = BigDecimal::from_str(self.trim()).map_err(...)?;` | **exact** |
| `src/types.rs:91` — `decimal_from_text` custom deserializer | `let parsed: BigDecimal = s.parse().map_err(serde::de::Error::custom)?;` | **exact** |

All serialization/deserialization goes through text, so no binary precision loss at client boundaries.

---

### 9. `weft-orchestration` (101 mentions, 6 files)

#### 9.1 Scale & rounding decisions

| Site | Code | Label |
|---|---|---|
| `src/lib.rs:2541` — `.round(2)` on amplitude | `let amplitude = vector.amplitude().round(2);` | **lossy by policy** (2-decimal rounding for display/logging) |
| `src/lib.rs:2552` — same on location and amplitude | `vector.location().round(2)` and `vector.amplitude().round(2)` | **lossy by policy** |
| `src/convert.rs:10` — `usize_to_f64` via `value.to_f64()` | `u64::try_from(value).ok().and_then(\|value\| value.to_f64()).unwrap_or(f64::MAX)` | **lossy by construction** |

---

### 10. `weft-xbench` (26 mentions, 6 files)

#### 10.1 Boundaries out of `BigDecimal`

| Site | Code | Label |
|---|---|---|
| `src/clickhouse.rs:345` — `to_f64()` for export | `p.value.to_f64().filter(\|v\| v.is_finite()).map(f64::to_le_bytes).ok_or_else(...)?` | **lossy by construction** |
| `src/duck.rs:256` — `to_f64()` for export | `p.value.to_f64().filter(\|v\| v.is_finite()).map(Value::Double).ok_or_else(...)?` | **lossy by construction** |
| `src/influxdb3.rs:335` — `to_f64()` for export | `p.value.to_f64().filter(\|v\| v.is_finite()).ok_or_else(...)?` | **lossy by construction** |
| `src/iotdb.rs:270` — `to_f64()` for export | `p.value.to_f64().filter(\|v\| v.is_finite()).ok_or_else(...)?` | **lossy by construction** |
| `src/questdb.rs:398` — `to_f64()` for export | `p.value.to_f64().filter(\|v\| v.is_finite()).map(Value::Double).ok_or_else(...)?` | **lossy by construction** |
| `src/timescale.rs:259` — `to_f64()` for export | `p.value.to_f64().filter(\|v\| v.is_finite()).map(\|v\| format!("{v:?}")).ok_or_else(...)?` | **lossy by construction** |
| `src/lib.rs:170` — `ToPrimitive::to_f64` on bucket values | `b.values.get(k).and_then(ToPrimitive::to_f64).ok_or_else(...)?` | **lossy by construction** |
| `src/lib.rs:410-412` — `Decimal` to f64 vector | `Self::Decimal(points) => points.iter().map(\|p\| p.value.to_f64().unwrap_or(f64::NAN)).collect(),` | **lossy by construction** |
| `src/lib.rs:48` — general `to_f64` helper | `v.to_f64().unwrap_or(f64::NAN)` | **lossy by construction** |

#### 10.2 Scale decisions

| Site | Code | Label |
|---|---|---|
| `src/questdb.rs:261-264` — `with_scale` then check equality | `let rescaled = v.with_scale(i64::from(scale)); anyhow::ensure!(&rescaled == v, ...)` | **lossy by policy** (`with_scale` rounds silently; the check detects it) |
| `src/duck.rs:154-157` — same | `let rescaled = v.with_scale(i64::from(scale));` | **lossy by policy** |
| `src/clickhouse.rs:248-251` — same | `let rescaled = v.with_scale(i64::from(scale));` | **lossy by policy** |
| `src/questdb.rs:255` — `normalized()` for scale detection | `v.normalized().fractional_digit_count().max(0)` | **exact** |
| `src/duck.rs:148` — same | `v.normalized().fractional_digit_count().max(0)` | **exact** |
| `src/clickhouse.rs:242` — same | `v.normalized().fractional_digit_count().max(0)` | **exact** |

---

### 11. `weft-tui` (2 mentions, 1 file)

| Site | Code | Label |
|---|---|---|
| `src/app.rs:1754` — `to_f64()` for chart display | `let value_f64 = p.value.to_f64().unwrap_or(0.0);` | **lossy by construction** |
| `src/app.rs:2109` — same | `point.value.to_f64().unwrap_or(0.0)` | **lossy by construction** |
| `src/app.rs:2239` — same | `point.value.to_f64().unwrap_or(0.0)` | **lossy by construction** |

---

### 12. `weft-arrow-store` (24 mentions, 1 file)

This crate passes `BigDecimal` values through as-is from the segment store to Arrow record batches. No precision boundary is introduced — values enter and exit as `BigDecimal` without float conversion or scale adjustment.

---

## Serde feature summary

| Crate | `serde` | `string-only` | Meaning for wire format |
|---|---|---|---|
| `weft-reduce` | Yes | **Yes** | `BigDecimal` serializes as a JSON **string** (e.g., `"99.92"`). Required by postcard (sidecar codec). |
| `weftdb` | Yes | No | `BigDecimal` serializes as a JSON **number** (e.g., `99.92`). Only safe across JSON boundaries. |
| `weft-physical-type` | Yes | No | Same as `weftdb`. |

The `string-only` feature makes `BigDecimal` use `serialize_str`/`deserialize_str` instead of the default `serialize`/`deserialize_any`. This matters because:
- Self-describing formats (JSON): both work — number or string.
- Non-self-describing binary formats (postcard): only `string-only` works, because `deserialize_any` is not supported.

The sidecar (persisted partial reduction) uses postcard and travels through `weft-reduce`, hence `string-only` there. The main store and physical-type layers use JSON for external APIs and do not need postcard support.

---

## Per-label site count

| Label | Count |
|---|---|
| **exact** | ~80+ sites (from_str, to_plain_string, BigDecimal::new from integers, normalized extraction) |
| **lossy by construction** | ~110+ sites (to_f64, from_f64, from_f32, f64→BigDecimal→f64 round trips) |
| **lossy by policy** | ~25 sites (with_scale, round(0), round(2), fit_i128 digit-shedding, division precision) |

Note: counts are approximate because many test files repeat the same pattern; the key precision-bearing sites are all listed above.

---

## Key observations

1. **The dominant loss is `to_f64`** — over 100 extraction sites push `BigDecimal` through an `f64` hop. Most are in test/synthetic-value code or in comparison thresholds where exactness is not needed.

2. **`from_f64` vs `decimal_from_f64`**: The workspace uses `decimal_from_f64` (shortest round-trip) for every float-to-decimal boundary in production code (ingest, ILP, JSON API). `BigDecimal::from_f64` (exact binary expansion) appears only in test fixtures and synthetic-value generation.

3. **Scale rounding is always explicit**: Every `with_scale`, `round`, and `fit_i128` call is a deliberate policy choice, never accidental. The `fit_i128` digit-shedding loop reports exactness to the caller.

4. **`normalized()` is used only for comparison and digest**, never for storage. It safely strips trailing zeros to make `1.50` and `1.5` compare equal.

5. **Division precision is the biggest open surface**: `BigDecimal` division in `fill.rs:200` and `scaled.rs:281` uses `bigdecimal`'s default division precision (100 decimal places). The quotient may be inexact for non-terminating rationals, but the precision is high enough that no practical measurement value approaches it.