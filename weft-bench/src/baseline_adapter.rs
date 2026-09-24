//! Portable linear-interpolation baseline adapter.
//!
//! The roadmap's fair-protocol (Phase 1.2) asks every interpolation comparison to
//! report, where possible, a **portable client-side baseline** — class (C): "fetch
//! raw → interpolate in the same Rust/Python client → total end-to-end". This
//! adapter *is* that baseline: a dependency-free, straightforward piecewise-linear
//! reconstruction implemented directly in the harness, with no engine
//! sophistication behind it. It is the honest reference WeftDB's native path is
//! measured against — until now Weft-Bench had only one system ([`WeftAdapter`]) and
//! so could not produce a *comparison* at all.
//!
//! Two deliberate properties keep the baseline trustworthy:
//!
//! * **Always linear.** The adapter ignores the requested [`Spline`] and always
//!   reconstructs with straight-line segments — that is the whole point of a
//!   *baseline*. Comparing WeftDB's chosen method against it is a quality/speed
//!   reference, not an apples-to-apples spline race; the name (`baseline-linear`)
//!   and this doc say so plainly rather than pretending otherwise.
//! * **Precision-aware, not precision-taxed.** Interpolation runs in
//!   [`BigDecimal`] end-to-end (the time ratio is an exact `i64`-nanosecond
//!   rational), so the baseline never silently downcasts through `f64` — honouring
//!   the same hard constraint as the rest of WeftDB. Values therefore stay finite and
//!   pass the harness correctness gate exactly as the WeftDB path does.

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{generate_target_times, Point, Resolution, Spline};

use crate::adapter::SystemAdapter;

/// Adapter that reconstructs a dense regular grid with piecewise-linear
/// interpolation — the portable client-side baseline WeftDB is compared against.
#[derive(Debug, Default, Clone, Copy)]
pub struct BaselineLinearAdapter;

impl BaselineLinearAdapter {
	/// Construct a baseline-linear adapter.
	#[must_use]
	pub const fn new() -> Self {
		Self
	}
}

#[async_trait]
impl SystemAdapter for BaselineLinearAdapter {
	fn name(&self) -> &'static str {
		"baseline-linear"
	}

	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, _spline: Spline) -> anyhow::Result<Vec<Point>> {
		anyhow::ensure!(!points.is_empty(), "baseline linear interpolation requires at least one input point");

		// The harness hands over a fresh clone each rep, so sorting in place is
		// safe and lets the bracketing search below assume ascending timestamps.
		points.sort_by_key(|p| p.timestamp);

		let grid = generate_target_times(start, end, resolution);
		let mut out = Vec::with_capacity(grid.len());
		for t in grid {
			out.push(Point::new(t, linear_value_at(points, t)));
		}
		Ok(out)
	}
}

/// Piecewise-linear value at `t` over ascending, non-empty `points`.
///
/// Inside the data range the value is interpolated from the bracketing pair;
/// outside it, the nearest end segment's slope is extrapolated (matching how a
/// linear reconstruction behaves past its first/last sample). A single input
/// point degenerates to a constant.
fn linear_value_at(points: &[Point], t: DateTime<Utc>) -> BigDecimal {
	if points.len() == 1 {
		return points[0].value.clone();
	}
	let last = points.len() - 1;
	match points.binary_search_by(|p| p.timestamp.cmp(&t)) {
		// Exact sample hit: return it verbatim, no arithmetic.
		Ok(i) => points[i].value.clone(),
		// Before the first sample: extrapolate along the first segment.
		Err(0) => interpolate_segment(&points[0], &points[1], t),
		// At or past the last sample: extrapolate along the final segment.
		Err(i) if i > last => interpolate_segment(&points[last - 1], &points[last], t),
		// Between samples `i-1` and `i`.
		Err(i) => interpolate_segment(&points[i - 1], &points[i], t),
	}
}

/// Linear value at `t` along the segment `a → b`, computed entirely in
/// [`BigDecimal`] so there is no float drift. A zero-or-unrepresentable time span
/// (duplicate stamps, or a span beyond `i64` nanoseconds) degenerates to `a`'s
/// value rather than dividing by zero.
fn interpolate_segment(a: &Point, b: &Point, t: DateTime<Utc>) -> BigDecimal {
	let span_ns = (b.timestamp - a.timestamp).num_nanoseconds();
	let offset_ns = (t - a.timestamp).num_nanoseconds();
	match (span_ns, offset_ns) {
		(Some(span), Some(offset)) if span != 0 => {
			let ratio = BigDecimal::from(offset) / BigDecimal::from(span);
			let v0 = a.value.clone();
			let v1 = b.value.clone();
			v0.clone() + (v1 - v0) * ratio
		}
		_ => a.value.clone(),
	}
}

#[cfg(test)]
mod tests {
	use bigdecimal::FromPrimitive;
	use chrono::TimeZone;

	use super::*;

	fn ts(secs: i64) -> DateTime<Utc> {
		Utc.timestamp_opt(secs, 0).single().expect("valid timestamp")
	}

	fn pt(secs: i64, value: f64) -> Point {
		Point::new(ts(secs), BigDecimal::from_f64(value).expect("finite value"))
	}

	/// Convert a small test-loop index to `f64` without a lossy `as` cast.
	fn idx(i: usize) -> f64 {
		f64::from(u16::try_from(i).expect("test index fits in u16"))
	}

	fn approx(value: &BigDecimal, want: f64) -> bool {
		use bigdecimal::ToPrimitive;
		(value.to_f64().expect("finite") - want).abs() < 1e-9
	}

	#[tokio::test]
	async fn fills_a_regular_grid_between_two_points() {
		// Two samples ten seconds apart; a one-second grid must produce eleven
		// points rising linearly from 0 to 10.
		let mut points = vec![pt(0, 0.0), pt(10, 10.0)];
		let out = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(0), ts(10), Resolution::Seconds, Spline::Linear).await.expect("interpolates");

		assert_eq!(out.len(), 11, "inclusive one-second grid over [0, 10]");
		for (i, p) in out.iter().enumerate() {
			assert!(approx(&p.value, idx(i)), "point {i} should equal {i}, got {}", p.value);
		}
	}

	#[tokio::test]
	async fn interpolates_the_midpoint_of_an_irregular_segment() {
		// A gap from t=0 (v=4) to t=4 (v=12): the midpoint t=2 must read 8.
		let mut points = vec![pt(0, 4.0), pt(4, 12.0)];
		let out = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(2), ts(2), Resolution::Seconds, Spline::Linear).await.expect("interpolates");
		assert_eq!(out.len(), 1);
		assert!(approx(&out[0].value, 8.0), "midpoint should be 8, got {}", out[0].value);
	}

	#[tokio::test]
	async fn sorts_out_of_order_input_before_interpolating() {
		// Reversed / shuffled input (as a TSBS payload may arrive) must yield the
		// same monotone ramp as sorted input.
		let mut points = vec![pt(10, 10.0), pt(0, 0.0), pt(5, 5.0)];
		let out = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(0), ts(10), Resolution::Seconds, Spline::Linear).await.expect("interpolates");
		assert_eq!(out.len(), 11);
		assert!(approx(&out[3].value, 3.0));
		assert!(approx(&out[7].value, 7.0));
	}

	#[tokio::test]
	async fn extrapolates_linearly_past_the_data_range() {
		// Grid extends two seconds before the first sample and two after the last;
		// the baseline continues each end segment's slope of 1/sec.
		let mut points = vec![pt(2, 2.0), pt(4, 4.0)];
		let out = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(0), ts(6), Resolution::Seconds, Spline::Linear).await.expect("interpolates");
		assert_eq!(out.len(), 7);
		assert!(approx(&out[0].value, 0.0), "extrapolated start should be 0, got {}", out[0].value);
		assert!(approx(&out[6].value, 6.0), "extrapolated end should be 6, got {}", out[6].value);
	}

	#[tokio::test]
	async fn single_point_degenerates_to_a_constant() {
		let mut points = vec![pt(5, 42.0)];
		let out = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(0), ts(3), Resolution::Seconds, Spline::Linear).await.expect("interpolates");
		assert_eq!(out.len(), 4);
		assert!(out.iter().all(|p| approx(&p.value, 42.0)), "all values constant at 42");
	}

	#[tokio::test]
	async fn empty_input_is_rejected() {
		let mut points: Vec<Point> = Vec::new();
		let err = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(0), ts(3), Resolution::Seconds, Spline::Linear).await;
		assert!(err.is_err(), "an empty series cannot be interpolated");
	}

	#[tokio::test]
	async fn ignores_requested_spline_and_stays_linear() {
		// Asking for Cubic must not change the (linear) result: the baseline is
		// linear by definition regardless of the requested method.
		let mut points = vec![pt(0, 0.0), pt(10, 10.0)];
		let out = BaselineLinearAdapter::new().interpolate_range(&mut points, ts(0), ts(10), Resolution::Seconds, Spline::Cubic).await.expect("interpolates");
		for (i, p) in out.iter().enumerate() {
			assert!(approx(&p.value, idx(i)), "cubic request still yields linear ramp");
		}
		assert_eq!(BaselineLinearAdapter::new().name(), "baseline-linear");
	}
}
