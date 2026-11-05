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
//! **Input cap.** A request is refused when its read window holds more than [`MAX_KNOTS`]
//! stored rows, so the knots handed to splimes (and the second copy built for them) are
//! bounded; splimes' `max_points` bounds the output grid. The check runs on the rows
//! [`SegmentStore::read_time_range`] returns, so the decode itself is bounded only by the
//! window: refusing before decoding needs the index's per-segment row counts, which are private
//! to `segment_store.rs` (the store lane's file), so that tightening belongs to that lane.
//!
//! The two refusals a caller may want to answer differently from a failure (the cap, and a
//! window with no stored sample) are a typed [`StoredWindowError`] inside the `anyhow` chain,
//! so a caller can `downcast_ref` them; the HTTP route answers the cap with
//! `422 result_too_large`.
//!
//! [`SegmentStore::interpolate_range_f64`] is the same reconstruction with `f64` values out.
//! splimes computes every method in `f64` and its `BigDecimal` result is the shortest decimal
//! that round-trips each computed `f64`, so for interpolated and extrapolated points the
//! `f64` result carries exactly the same information; only a raw point, which the
//! `BigDecimal` result returns as the stored value exactly, is rounded to the nearest `f64`.
//! Building a `BigDecimal` per output point is most of the `BigDecimal` call's cost (on the
//! bench, ~1M points: 38.0 ms against 7.7 ms for the `f64` call on rayon), so a caller whose
//! own boundary is `f64` (a JSON body, Arrow `Float64`, a plot) should take this one.

use std::ops::ControlFlow;

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{Interpolation, Interpolator, Point, PointKind, StreamSummary, TickUnit};
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

/// splimes' tick length for a stored time unit.
const fn tick_unit(unit: TimeUnit) -> TickUnit {
	match unit {
		TimeUnit::Seconds => TickUnit::SECONDS,
		TimeUnit::Millis => TickUnit::MILLISECONDS,
		TimeUnit::Micros => TickUnit::MICROSECONDS,
		TimeUnit::Nanos => TickUnit::NANOSECONDS,
	}
}

/// The present rows as `f64` columns, ticks kept as stored (no `DateTime` per row).
fn f64_columns(timestamps: Vec<i64>, values: Vec<Option<BigDecimal>>) -> Result<(Vec<i64>, Vec<f64>)> {
	let (mut ticks, mut floats) = (Vec::with_capacity(timestamps.len()), Vec::with_capacity(timestamps.len()));
	for (t, v) in timestamps.into_iter().zip(values) {
		if let Some(value) = v {
			ticks.push(t);
			// The same `f64` as `to_f64`, without formatting and parsing a short decimal.
			floats.push(weft_reduce::decimal_to_f64(&value).filter(|f| f.is_finite()).with_context(|| format!("stored value {value} has no finite f64 image"))?);
		}
	}
	Ok((ticks, floats))
}

/// One streamed chunk of a stored reconstruction: grid points `first..first + len()`, owned
/// (copied out of splimes' reused buffers), times as ticks of [`StoredStream::tick_unit`].
#[derive(Debug, Clone, PartialEq)]
pub struct StoredChunk {
	/// The grid index of the chunk's first point.
	pub first: usize,
	/// The grid times, ascending, in ticks of the stream's [`tick_unit`](StoredStream::tick_unit).
	pub ticks: Vec<i64>,
	/// The values, one per time.
	pub values: Vec<f64>,
	/// Provenance, one per time.
	pub kinds: Vec<PointKind>,
}

impl StoredChunk {
	/// The number of points in the chunk.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.values.len()
	}

	/// Whether the chunk holds no point (a streamed chunk never is empty).
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.values.is_empty()
	}
}

/// A stored reconstruction delivered in chunks as splimes computes them (P5-5.3).
///
/// The interpolation runs on the blocking pool and hands each chunk to a bounded channel of
/// `depth` chunks, so at most `depth + 1` chunks (plus the one being serialised) are alive at a
/// time, however large the grid: a caller serialises chunk `k` while the kernel computes chunk
/// `k + 1` (on the GPU, splimes overlaps the read-back of one chunk with the next's compute).
/// Dropping the stream stops the run at the next chunk boundary.
#[derive(Debug)]
pub struct StoredStream {
	unit: TickUnit,
	chunks: tokio::sync::mpsc::Receiver<StoredChunk>,
	run: tokio::task::JoinHandle<std::result::Result<StreamSummary, splimes::Error>>,
}

impl StoredStream {
	/// The tick length of every chunk's [`ticks`](StoredChunk::ticks): the aspect's declared
	/// unit, or nanoseconds when the grid step is not a whole number of the aspect's ticks
	/// (a millisecond grid over a seconds aspect).
	#[must_use]
	pub const fn tick_unit(&self) -> TickUnit {
		self.unit
	}

	/// The next chunk, in grid order; `None` once the run has delivered everything it will
	/// (call [`finish`](Self::finish) to learn whether that was all of it).
	pub async fn next(&mut self) -> Option<StoredChunk> {
		self.chunks.recv().await
	}

	/// Wait for the run and return its summary (spline, backend, points delivered).
	///
	/// # Errors
	///
	/// splimes' error if the run failed (chunks before the failing point may already have been
	/// delivered, as [`Interpolator::stream_epoch`] documents), or if the task panicked.
	pub async fn finish(self) -> Result<StreamSummary> {
		let Self { chunks, run, .. } = self;
		// Closing the receiver lets a run still blocked on a full channel stop at once.
		drop(chunks);
		Ok(run.await.context("the interpolation task panicked")??)
	}
}

/// The tick unit a stream runs in and the knots, start and end in it: the aspect's own unit
/// when the grid step is a whole number of its ticks, else nanoseconds.
fn stream_ticks(unit: TimeUnit, step_nanos: i64, ticks: Vec<i64>, start: i64, end: i64) -> Result<(TickUnit, Vec<i64>, i64, i64)> {
	let native = tick_unit(unit);
	let per_tick = i64::try_from(native.nanos_per_tick()).unwrap_or(i64::MAX);
	if step_nanos % per_tick == 0 {
		return Ok((native, ticks, start, end));
	}
	let scale = |t: i64| t.checked_mul(per_tick).with_context(|| format!("epoch {t} in {} has no nanosecond tick; a grid finer than the aspect's unit needs nanosecond instants", unit.name()));
	let scaled = ticks.into_iter().map(scale).collect::<Result<Vec<i64>>>()?;
	Ok((TickUnit::NANOSECONDS, scaled, scale(start)?, scale(end)?))
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

/// The most stored rows one reconstruction reads (null rows count).
///
/// 4 Mi, about 40 times the 100k-knot window the bench measures. A wider window is refused: narrow
/// `[start, end]` or `margin`, or interpolate in pieces.
pub const MAX_KNOTS: usize = 4 << 20;

/// A stored window that cannot be reconstructed as asked: carried inside the `anyhow` error
/// the reconstruction calls return, for a caller that answers these apart from a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoredWindowError {
	/// `[lo, hi]` (the window widened by the margin) holds more stored rows than one
	/// reconstruction reads.
	TooManyRows {
		/// The aspect read.
		aspect: String,
		/// The read window's first instant (`start − margin`, saturating).
		lo: i64,
		/// The read window's last instant (`end + margin`, saturating).
		hi: i64,
		/// The stored rows (null rows included) in `[lo, hi]`.
		rows: usize,
		/// The cap, [`MAX_KNOTS`] outside tests.
		limit: usize,
	},
	/// `[lo, hi]` holds no stored sample with a value, so there is nothing to interpolate.
	NoSample {
		/// The aspect read.
		aspect: String,
		/// The read window's first instant.
		lo: i64,
		/// The read window's last instant.
		hi: i64,
	},
}

impl std::fmt::Display for StoredWindowError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::TooManyRows { aspect, lo, hi, rows, limit } => write!(f, "interpolate_range: [{lo}, {hi}] holds {rows} stored rows of aspect {aspect:?}, more than the {limit} one reconstruction reads; narrow the window or the margin"),
			Self::NoSample { aspect, lo, hi } => write!(f, "aspect {aspect:?} has no stored sample in [{lo}, {hi}]"),
		}
	}
}

impl std::error::Error for StoredWindowError {}

impl SegmentStore {
	/// Validate a reconstruction request and read its rows.
	async fn stored_window(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, margin: i64) -> Result<StoredWindow> {
		self.stored_window_capped(aspect, series, start, end, margin, MAX_KNOTS).await
	}

	/// [`stored_window`](Self::stored_window) with an explicit row cap (tests lower it).
	async fn stored_window_capped(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, margin: i64, max_rows: usize) -> Result<StoredWindow> {
		anyhow::ensure!(start <= end, "interpolate_range: start {start} is after end {end}");
		anyhow::ensure!(margin >= 0, "interpolate_range: margin {margin} is negative");
		let schema = self.schema_for(aspect).await?.with_context(|| format!("aspect {aspect:?} is not declared"))?;
		let unit = schema.timestamp_unit;
		let (from, to) = (instant(start, unit).with_context(|| format!("start {start} is outside the representable range"))?, instant(end, unit).with_context(|| format!("end {end} is outside the representable range"))?);
		let (lo, hi) = (start.saturating_sub(margin), end.saturating_add(margin));
		let (timestamps, values) = self.read_time_range_of(aspect, series, lo, hi).await?;
		if timestamps.len() > max_rows {
			return Err(StoredWindowError::TooManyRows { aspect: aspect.to_string(), lo, hi, rows: timestamps.len(), limit: max_rows }.into());
		}
		if !values.iter().any(Option::is_some) {
			return Err(StoredWindowError::NoSample { aspect: aspect.to_string(), lo, hi }.into());
		}
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
	/// bound or stored instant is outside chrono's range, the window holds no stored sample or
	/// more than [`MAX_KNOTS`] rows, the read fails, or splimes rejects the interpolation (for example a grid larger than
	/// the interpolator's `max_points`, or `Backend::Gpu` without a usable GPU).
	pub async fn interpolate_range(&self, aspect: &str, start: i64, end: i64, margin: i64, interpolator: Interpolator) -> Result<Interpolation<BigDecimal>> {
		self.interpolate_range_of(aspect, crate::SeriesId::UNTAGGED, start, end, margin, interpolator).await
	}

	/// [`interpolate_range`](Self::interpolate_range) of one series (TAG-8): only the
	/// samples of `series` are knots.
	///
	/// # Errors
	///
	/// As [`interpolate_range`](Self::interpolate_range).
	pub async fn interpolate_range_of(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, margin: i64, interpolator: Interpolator) -> Result<Interpolation<BigDecimal>> {
		let StoredWindow { from, to, unit, timestamps, values } = self.stored_window(aspect, series, start, end, margin).await?;
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
		self.interpolate_range_f64_of(aspect, crate::SeriesId::UNTAGGED, start, end, margin, interpolator).await
	}

	/// [`interpolate_range_f64`](Self::interpolate_range_f64) of one series (TAG-8).
	///
	/// # Errors
	///
	/// As [`interpolate_range_f64`](Self::interpolate_range_f64).
	pub async fn interpolate_range_f64_of(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, margin: i64, interpolator: Interpolator) -> Result<Interpolation<f64>> {
		let StoredWindow { unit, timestamps, values, .. } = self.stored_window(aspect, series, start, end, margin).await?;
		let (ticks, floats) = f64_columns(timestamps, values)?;
		// Epoch-native input (splimes 1.3, `run_epoch`): no `DateTime` is built per knot, and
		// the result is bit-identical to `run_f64` on the same instants.
		tokio::task::spawn_blocking(move || interpolator.run_epoch(&ticks, tick_unit(unit), &floats, start, end)).await.context("the interpolation task panicked")?.with_context(|| format!("interpolating aspect {aspect:?}"))
	}

	/// [`interpolate_range_f64`](Self::interpolate_range_f64), streamed: the grid comes back
	/// in chunks as it is computed, so a caller can serialise the first points while later
	/// ones are still computing, holding at most `depth` chunks in flight (see
	/// [`StoredStream`]). Chunk size is the interpolator's
	/// [`stream_chunk_points`](Interpolator::stream_chunk_points).
	///
	/// The knots go to splimes as epoch ticks ([`Interpolator::stream_epoch`]), with no
	/// `DateTime` per knot or per output point. Every time, value and kind is the one
	/// [`interpolate_range_f64`](Self::interpolate_range_f64) returns, bit for bit.
	///
	/// # Errors
	///
	/// The request and read errors of [`interpolate_range_f64`](Self::interpolate_range_f64)
	/// are returned here, before any chunk; an interpolation error comes from
	/// [`StoredStream::finish`].
	pub async fn interpolate_range_stream(&self, aspect: &str, start: i64, end: i64, margin: i64, interpolator: Interpolator, depth: usize) -> Result<StoredStream> {
		self.interpolate_range_stream_of(aspect, crate::SeriesId::UNTAGGED, start, end, margin, interpolator, depth).await
	}

	/// [`interpolate_range_stream`](Self::interpolate_range_stream) of one series (TAG-8).
	///
	/// # Errors
	///
	/// As [`interpolate_range_stream`](Self::interpolate_range_stream).
	#[allow(clippy::too_many_arguments)]
	pub async fn interpolate_range_stream_of(&self, aspect: &str, series: crate::SeriesId, start: i64, end: i64, margin: i64, interpolator: Interpolator, depth: usize) -> Result<StoredStream> {
		let StoredWindow { unit, timestamps, values, .. } = self.stored_window(aspect, series, start, end, margin).await?;
		let (ticks, floats) = f64_columns(timestamps, values)?;
		let (tick, ticks, start, end) = stream_ticks(unit, interpolator.resolution().step_nanos(), ticks, start, end)?;
		let (tx, chunks) = tokio::sync::mpsc::channel(depth.max(1));
		let run = tokio::task::spawn_blocking(move || {
			interpolator.stream_epoch(&ticks, tick, &floats, start, end, |chunk| {
				let owned = StoredChunk { first: chunk.first(), ticks: chunk.times().to_vec(), values: chunk.values().to_vec(), kinds: chunk.kinds().to_vec() };
				// A closed receiver (the caller went away) stops the run.
				if tx.blocking_send(owned).is_ok() {
					ControlFlow::Continue(())
				} else {
					ControlFlow::Break(())
				}
			})
		});
		Ok(StoredStream { unit: tick, chunks, run })
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
	async fn a_window_with_more_rows_than_the_cap_is_refused() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		// The store holds 20 rows; a cap of 20 admits the whole range and a cap of 19 does not.
		let whole = store.stored_window_capped("line", crate::SeriesId::UNTAGGED, 0, 190, 0, 20).await.expect("at the cap");
		assert_eq!(whole.timestamps.len(), 20);
		let err = store.stored_window_capped("line", crate::SeriesId::UNTAGGED, 0, 190, 0, 19).await.err().expect("over the cap").to_string();
		assert!(err.contains("holds 20 stored rows") && err.contains("narrow the window"), "{err}");
		// The margin counts: [95, 105] alone holds one row, with a margin of 50 it holds more.
		assert!(store.stored_window_capped("line", crate::SeriesId::UNTAGGED, 95, 105, 0, 1).await.is_ok());
		let padded = store.stored_window_capped("line", crate::SeriesId::UNTAGGED, 95, 105, 50, 5).await.err().expect("the margin counts toward the cap").to_string();
		assert!(padded.contains("holds 11 stored rows"), "{padded}");
		assert!(store.stored_window_capped("line", crate::SeriesId::UNTAGGED, 95, 105, 50, 11).await.is_ok());
		// The refusal is typed, so a caller can tell it from a failure.
		let typed = store.stored_window_capped("line", crate::SeriesId::UNTAGGED, 0, 190, 0, 19).await.err().expect("over the cap");
		assert_eq!(typed.downcast_ref::<StoredWindowError>(), Some(&StoredWindowError::TooManyRows { aspect: "line".to_string(), lo: 0, hi: 190, rows: 20, limit: 19 }));
		// The default cap is far above anything these tests read.
		const { assert!(MAX_KNOTS >= 4_000_000) };
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
		let empty = store.interpolate_range("line", 1_000, 2_000, 0, linear).await.unwrap_err();
		assert!(empty.to_string().contains("no stored sample"));
		assert!(matches!(empty.downcast_ref::<StoredWindowError>(), Some(StoredWindowError::NoSample { .. })));
		assert!(store.interpolate_range("line", 0, 190, 0, linear.max_points(10)).await.is_err(), "splimes' grid limit applies");
		drop(store);
	}

	/// Drain a stream into columns, checking chunk order on the way.
	async fn drain(mut stream: StoredStream) -> (TickUnit, Vec<i64>, Vec<f64>, Vec<PointKind>, StreamSummary) {
		let unit = stream.tick_unit();
		let (mut ticks, mut values, mut kinds) = (Vec::new(), Vec::new(), Vec::new());
		while let Some(chunk) = stream.next().await {
			assert_eq!(chunk.first, values.len(), "chunks arrive in grid order");
			assert!(!chunk.is_empty());
			ticks.extend(chunk.ticks);
			values.extend(chunk.values);
			kinds.extend(chunk.kinds);
		}
		let summary = stream.finish().await.expect("the run succeeds");
		(unit, ticks, values, kinds, summary)
	}

	#[tokio::test]
	async fn the_stream_is_the_f64_call_bit_for_bit() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		for spline in [Spline::Linear, Spline::Cubic] {
			// Chunks of 7 points over a 185-point grid: many chunks, the last one short.
			let interpolator = Interpolator::new(spline, Resolution::Seconds).stream_chunk_points(7);
			let whole = store.interpolate_range_f64("line", 3, 187, 20, interpolator).await.expect("interpolates");
			let (unit, ticks, values, kinds, summary) = drain(store.interpolate_range_stream("line", 3, 187, 20, interpolator, 2).await.expect("streams")).await;
			assert_eq!(unit, TickUnit::SECONDS, "a seconds grid over a seconds aspect stays in seconds");
			assert!(summary.is_complete() && summary.len() == whole.len() && summary.chunks() > 1, "{summary:?}");
			assert_eq!(ticks, (3..=187).collect::<Vec<i64>>());
			let instants: Vec<DateTime<Utc>> = ticks.iter().map(|&t| DateTime::from_timestamp(t, 0).unwrap()).collect();
			assert_eq!(instants.as_slice(), whole.timestamps());
			assert_eq!(kinds.as_slice(), whole.kinds());
			assert_eq!(values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), whole.values().iter().map(|v| v.to_bits()).collect::<Vec<_>>(), "{spline:?}");
		}
		drop(store);
	}

	#[tokio::test]
	async fn a_grid_finer_than_the_aspect_unit_streams_in_nanoseconds() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		let interpolator = Interpolator::new(Spline::Linear, Resolution::Milliseconds).stream_chunk_points(100);
		let whole = store.interpolate_range_f64("line", 99, 100, 10, interpolator).await.expect("interpolates");
		let (unit, ticks, values, kinds, _) = drain(store.interpolate_range_stream("line", 99, 100, 10, interpolator, 1).await.expect("streams")).await;
		assert_eq!(unit, TickUnit::NANOSECONDS);
		assert_eq!(ticks.len(), 1_001);
		assert_eq!((ticks[0], ticks[1_000]), (99_000_000_000, 100_000_000_000));
		let instants: Vec<DateTime<Utc>> = ticks.iter().map(|&t| DateTime::from_timestamp_nanos(t)).collect();
		assert_eq!(instants.as_slice(), whole.timestamps());
		assert_eq!(kinds.as_slice(), whole.kinds());
		assert_eq!(values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), whole.values().iter().map(|v| v.to_bits()).collect::<Vec<_>>());
		drop(store);
	}

	#[tokio::test]
	async fn dropping_the_receiver_stops_the_run_and_request_errors_come_first() {
		let dir = TempDir::new().expect("tempdir");
		let store = line_store(&dir).await;
		let interpolator = Interpolator::new(Spline::Linear, Resolution::Seconds).stream_chunk_points(1);
		let mut stream = store.interpolate_range_stream("line", 0, 190, 0, interpolator, 1).await.expect("streams");
		assert_eq!(stream.next().await.expect("a first chunk").first, 0);
		let summary = stream.finish().await.expect("a stopped run is not an error");
		assert!(!summary.is_complete() && summary.delivered() < 191, "{summary:?}");
		// Request and read refusals are errors of the call itself, before any chunk.
		assert!(store.interpolate_range_stream("nope", 0, 10, 0, interpolator, 1).await.is_err());
		let empty = store.interpolate_range_stream("line", 1_000, 2_000, 0, interpolator, 1).await.unwrap_err();
		assert!(matches!(empty.downcast_ref::<StoredWindowError>(), Some(StoredWindowError::NoSample { .. })));
		// splimes' own refusals come from `finish`.
		let capped = store.interpolate_range_stream("line", 0, 190, 0, interpolator.max_points(10), 1).await.expect("starts");
		assert!(capped.finish().await.is_err(), "splimes' grid limit applies");
		drop(store);
	}
}
