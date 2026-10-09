//! **Interpolate-on-read over stored segments**: a regular grid reconstructed from an
//! aspect's persisted samples, the storage-backed counterpart of the compute endpoints'
//! request-supplied interpolation.
//!
//! [`SegmentStore::interpolate_range`] prunes the index to the window, decodes only the
//! overlapping `.weftseg` frames ([`SegmentStore::read_time_range`]), lifts the present rows to
//! splimes knots in the aspect's declared time unit, and runs the caller's
//! [`Interpolator`] (spline, resolution, backend, GPU precision) on the blocking pool. Every
//! output point carries splimes' provenance (raw / interpolated / extrapolated).
//!
//! The knots are read from `[start − margin, end + margin]` (in the aspect's unit), not just
//! the grid window: a spline near either edge of the window needs the samples just outside
//! it, which a window-only read would miss and turn into an extrapolation. A margin of a few
//! sample spacings is enough for the local splimes methods (a cubic uses two knots on each
//! side). Samples at one instant in several segments are all passed to splimes in seal order,
//! and splimes keeps the last one given, so the newest write wins as it does elsewhere in the
//! store.
//!
//! [`SegmentStore::interpolate_range_f64`] is the same reconstruction with `f64` values out.
//! splimes computes every method in `f64` and its `BigDecimal` result is the shortest decimal
//! that round-trips each computed `f64`, so for interpolated and extrapolated points the
//! `f64` result carries exactly the same information; only a raw point, which the
//! `BigDecimal` result returns as the stored value exactly, is rounded to the nearest `f64`.
//! Building a `BigDecimal` per output point is most of the `BigDecimal` call's cost (on the
//! bench, ~1M points: 38.0 ms against 7.7 ms for the `f64` call on rayon), so a caller whose
//! own boundary is `f64` (a JSON body, Arrow `Float64`, a plot) should take this one.

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{Interpolation, Interpolator, Point};
use weft_physical_type::TimeUnit;

use crate::types::segment_store::SegmentStore;

/// The instant of `epoch` in `unit`, or `None` outside chrono's range (the same mapping the
/// store's read paths use).
const fn instant(epoch: i64, unit: TimeUnit) -> Option<DateTime<Utc>> {
	match unit {
		TimeUnit::Seconds => DateTime::<Utc>::from_timestamp(epoch, 0),
		TimeUnit::Millis => DateTime::<Utc>::from_timestamp_millis(epoch),
		TimeUnit::Micros => DateTime::<Utc>::from_timestamp_micros(epoch),
		TimeUnit::Nanos => Some(DateTime::<Utc>::from_timestamp_nanos(epoch)),
	}
}

/// The stored rows a reconstruction of `[start, end]` reads, with the grid bounds as instants.
struct StoredWindow {
	/// The grid's first and last instants.
	from: DateTime<Utc>,
	to: DateTime<Utc>,
	/// The aspect's declared time unit.
	unit: TimeUnit,
	/// The rows in `[start − margin, end + margin]`, as `read_time_range` returns them.
	timestamps: Vec<i64>,
	values: Vec<Option<BigDecimal>>,
}

impl SegmentStore {
	/// Validate a reconstruction request and read its rows.
	async fn stored_window(&self, aspect: &str, start: i64, end: i64, margin: i64) -> Result<StoredWindow> {
		anyhow::ensure!(start <= end, "interpolate_range: start {start} is after end {end}");
		anyhow::ensure!(margin >= 0, "interpolate_range: margin {margin} is negative");
		let schema = self.schema_for(aspect).await?.with_context(|| format!("aspect {aspect:?} is not declared"))?;
		let unit = schema.timestamp_unit;
		let (from, to) = (instant(start, unit).with_context(|| format!("start {start} is outside the representable range"))?, instant(end, unit).with_context(|| format!("end {end} is outside the representable range"))?);
		let (lo, hi) = (start.saturating_sub(margin), end.saturating_add(margin));
		let (timestamps, values) = self.read_time_range(aspect, lo, hi).await?;
		anyhow::ensure!(values.iter().any(Option::is_some), "aspect {aspect:?} has no stored sample in [{lo}, {hi}]");
		Ok(StoredWindow { from, to, unit, timestamps, values })
	}

	/// Reconstruct `aspect` on `interpolator`'s regular grid from `start` to `end`
	/// (inclusive, in the aspect's declared time unit; the grid is anchored at `start`), from
	/// the stored samples in `[start − margin, end + margin]`.
	///
	/// Null rows are skipped. See the [module docs](self) for the margin and for samples
	/// that share an instant.
	///
	/// # Errors
	///
	/// If `aspect` is undeclared or its name invalid, `start > end`, `margin` is negative, a
	/// bound or stored instant is outside chrono's range, the window holds no stored sample,
	/// the read fails, or splimes rejects the interpolation (for example a grid larger than
	/// the interpolator's `max_points`, or `Backend::Gpu` without a usable GPU).
	pub async fn interpolate_range(&self, aspect: &str, start: i64, end: i64, margin: i64, interpolator: Interpolator) -> Result<Interpolation<BigDecimal>> {
		let StoredWindow { from, to, unit, timestamps, values } = self.stored_window(aspect, start, end, margin).await?;
		let knots = timestamps.into_iter().zip(values).filter_map(|(t, v)| v.map(|value| instant(t, unit).map(|at| Point::new(at, value)).with_context(|| format!("stored epoch {t} is outside the representable range")))).collect::<Result<Vec<Point>>>()?;
		tokio::task::spawn_blocking(move || interpolator.run(&knots, from, to)).await.context("the interpolation task panicked")?.with_context(|| format!("interpolating aspect {aspect:?}"))
	}

	/// [`interpolate_range`](Self::interpolate_range) with `f64` values out (see the
	/// [module docs](self) for what differs: only raw points, rounded to the nearest `f64`).
	///
	/// # Errors
	///
	/// As [`interpolate_range`](Self::interpolate_range), and if a stored value has no finite
	/// `f64` image.
	pub async fn interpolate_range_f64(&self, aspect: &str, start: i64, end: i64, margin: i64, interpolator: Interpolator) -> Result<Interpolation<f64>> {
		let StoredWindow { from, to, unit, timestamps, values } = self.stored_window(aspect, start, end, margin).await?;
		let (mut instants, mut floats) = (Vec::with_capacity(timestamps.len()), Vec::with_capacity(timestamps.len()));
		for (t, v) in timestamps.into_iter().zip(values) {
			if let Some(value) = v {
				instants.push(instant(t, unit).with_context(|| format!("stored epoch {t} is outside the representable range"))?);
				// The same `f64` as `to_f64`, without formatting and parsing a short decimal.
				floats.push(weft_reduce::decimal_to_f64(&value).filter(|f| f.is_finite()).with_context(|| format!("stored value {value} has no finite f64 image"))?);
			}
		}
		tokio::task::spawn_blocking(move || interpolator.run_f64(&instants, &floats, from, to)).await.context("the interpolation task panicked")?.with_context(|| format!("interpolating aspect {aspect:?}"))
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::ToPrimitive;
	use splimes::{PointKind, Resolution, Spline};
	use tempfile::TempDir;
	use weft_physical_type::{AspectSchema, PhysicalType};

	use super::*;

	fn dec(s: &str) -> BigDecimal {
		BigDecimal::from_str(s).expect("decimal")
	}

	/// A store with aspect `line` (`ScaledI64` scale 2, seconds) holding v = 2·t at every
	/// tenth second from 0 to 190, sealed as two segments that meet at t = 100.
	async fn line_store(dir: &TempDir) -> SegmentStore {
		let store = SegmentStore::open(dir.path()).await.expect("opens");
		store.declare("line", &AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, BigDecimal::from(0), TimeUnit::Seconds)).await.expect("declares");
		for half in [0_i64, 1] {
			let ts: Vec<i64> = (0..10).map(|i| (half * 10 + i) * 10).collect();
			let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(2 * t)).collect();
			store.seal_declared("line", &ts, &vs).await.expect("seals");
		}
		store
	}

	#[tokio::test]
	async fn a_window_across_two_segments_is_reconstructed_with_provenance() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		let linear = Interpolator::new(Spline::Linear, Resolution::Seconds);
		let series = store.interpolate_range("line", 95, 105, 20, linear).await.expect("interpolates");
		assert_eq!(series.len(), 11);
		for (i, (value, kind)) in series.values().iter().zip(series.kinds()).enumerate() {
			let t = 95 + i64::try_from(i).unwrap();
			assert_eq!(value, &BigDecimal::from(2 * t), "v = 2t at {t}");
			assert_eq!(*kind, if t == 100 { PointKind::Raw } else { PointKind::Interpolated }, "at {t}");
		}
		assert_eq!(series.timestamps()[0], DateTime::from_timestamp(95, 0).unwrap(), "the grid is anchored at start");
		drop(store);
	}

	#[tokio::test]
	async fn the_margin_turns_an_edge_extrapolation_into_an_interpolation() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		let linear = Interpolator::new(Spline::Linear, Resolution::Seconds);
		// [95, 105] holds only the knot at 100; without a margin every other point is outside
		// the data, and splimes holds the single knot's value.
		let bare = store.interpolate_range("line", 95, 105, 0, linear).await.expect("interpolates");
		assert_eq!(bare.kinds()[0], PointKind::Extrapolated);
		assert_eq!(bare.values()[0], dec("200"));
		let padded = store.interpolate_range("line", 95, 105, 10, linear).await.expect("interpolates");
		drop(store);
		assert_eq!((padded.kinds()[0], &padded.values()[0]), (PointKind::Interpolated, &dec("190")));
	}

	#[tokio::test]
	async fn the_f64_call_returns_the_bigdecimal_calls_values_as_f64() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		for spline in [Spline::Linear, Spline::Cubic] {
			let interpolator = Interpolator::new(spline, Resolution::Seconds);
			let exact = store.interpolate_range("line", 3, 187, 20, interpolator).await.expect("interpolates");
			let float = store.interpolate_range_f64("line", 3, 187, 20, interpolator).await.expect("interpolates");
			assert_eq!((exact.timestamps(), exact.kinds()), (float.timestamps(), float.kinds()));
			for (e, f) in exact.values().iter().zip(float.values()) {
				// The BigDecimal is the shortest decimal of the computed f64: it parses back to it.
				assert_eq!(e.to_f64().expect("finite").to_bits(), f.to_bits(), "{e} vs {f}");
			}
		}
		assert!(store.interpolate_range_f64("nope", 0, 10, 0, Interpolator::new(Spline::Linear, Resolution::Seconds)).await.is_err());
		drop(store);
	}

	#[tokio::test]
	async fn bad_requests_are_errors() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		let linear = Interpolator::new(Spline::Linear, Resolution::Seconds);
		assert!(store.interpolate_range("nope", 0, 10, 0, linear).await.unwrap_err().to_string().contains("not declared"));
		assert!(store.interpolate_range("line", 10, 0, 0, linear).await.is_err(), "start after end");
		assert!(store.interpolate_range("line", 0, 10, -1, linear).await.is_err(), "negative margin");
		assert!(store.interpolate_range("line", 1_000, 2_000, 0, linear).await.unwrap_err().to_string().contains("no stored sample"));
		assert!(store.interpolate_range("line", 0, 190, 0, linear.max_points(10)).await.is_err(), "splimes' grid limit applies");
		drop(store);
	}
}
