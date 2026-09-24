//! Portable forward-fill (LOCF) baseline adapter.
//!
//! The roadmap's fair-protocol (Phase 1.2) asks every interpolation comparison to
//! report, where possible, the **native in-DB gap-fill** behaviour alongside WeftDB's
//! interpolation. The single most common such behaviour across time-series engines
//! is *last-observation-carried-forward* (LOCF): `InfluxDB`'s `FILL(previous)`,
//! `QuestDB`'s `FILL(prev)`, and `TimescaleDB`'s `locf()` all reconstruct a gap by
//! holding the most recent prior sample until the next one arrives. This adapter
//! reproduces that step-function reconstruction **portably**, in-process, with no
//! engine behind it — so a Weft-Bench comparison can carry WeftDB's interpolation
//! against the gap-fill real databases actually ship, not only the linear baseline
//! in [`BaselineLinearAdapter`](crate::baseline_adapter::BaselineLinearAdapter).
//!
//! Two deliberate properties keep the baseline trustworthy:
//!
//! * **Always forward-fill.** The adapter ignores the requested [`Spline`] and
//!   always reconstructs with a piecewise-constant step (hold the latest sample).
//!   That is the whole point of a *baseline*: comparing WeftDB's chosen method against
//!   it is a quality/speed reference, not an apples-to-apples spline race; the name
//!   (`baseline-forward-fill`) and this doc say so plainly.
//! * **No fabricated precision.** A held value is the prior sample's exact
//!   [`BigDecimal`] — copied verbatim, never arithmetic — so the baseline cannot
//!   introduce float drift and every produced value stays finite, passing the
//!   harness correctness gate exactly as the WeftDB path does.
//!
//! Boundary convention: before the first sample there is no prior observation to
//! carry forward. Rather than emit a null (which the correctness gate forbids), the
//! baseline holds the *first* sample backward over that leading region — the
//! standard finite-valued choice when a back-fill is unavailable. At or after a
//! sample the value steps to that sample and holds until the next.

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use splimes::{generate_target_times, Point, Resolution, Spline};

use crate::adapter::SystemAdapter;

/// Adapter that reconstructs a dense regular grid by carrying the most recent
/// sample forward (LOCF) — the portable mirror of native TSDB `FILL(previous)`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ForwardFillAdapter;

impl ForwardFillAdapter {
	/// Construct a forward-fill (LOCF) baseline adapter.
	#[must_use]
	pub const fn new() -> Self {
		Self
	}
}

#[async_trait]
impl SystemAdapter for ForwardFillAdapter {
	fn name(&self) -> &'static str {
		"baseline-forward-fill"
	}

	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, _spline: Spline) -> anyhow::Result<Vec<Point>> {
		anyhow::ensure!(!points.is_empty(), "forward-fill interpolation requires at least one input point");

		// The harness hands over a fresh clone each rep, so sorting in place is
		// safe and lets the bracketing search below assume ascending timestamps.
		points.sort_by_key(|p| p.timestamp);

		let grid = generate_target_times(start, end, resolution);
		let mut out = Vec::with_capacity(grid.len());
		for t in grid {
			out.push(Point::new(t, forward_fill_value_at(points, t)));
		}
		Ok(out)
	}
}

/// Last-observation-carried-forward value at `t` over ascending, non-empty
/// `points`: the value of the latest sample whose timestamp is `<= t`. Before the
/// first sample (no prior observation) the first sample's value is held backward so
/// the result is always a finite, real value.
fn forward_fill_value_at(points: &[Point], t: DateTime<Utc>) -> BigDecimal {
	match points.binary_search_by(|p| p.timestamp.cmp(&t)) {
		// Exact sample hit: that sample's value.
		Ok(i) => points[i].value.clone(),
		// Before the first sample: hold the first value backward (no prior exists).
		Err(0) => points[0].value.clone(),
		// `i` is the insertion point, so `i - 1` is the latest sample at or before
		// `t` — including `i == len`, where it is the final sample.
		Err(i) => points[i - 1].value.clone(),
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

	fn approx(value: &BigDecimal, want: f64) -> bool {
		use bigdecimal::ToPrimitive;
		(value.to_f64().expect("finite") - want).abs() < 1e-9
	}

	#[tokio::test]
	async fn holds_each_sample_until_the_next() {
		// Samples at t=0 (v=10) and t=4 (v=20): a one-second grid must step — 10
		// held across t=0..3, then 20 from t=4 — never interpolating between.
		let mut points = vec![pt(0, 10.0), pt(4, 20.0)];
		let out = ForwardFillAdapter::new().interpolate_range(&mut points, ts(0), ts(4), Resolution::Seconds, Spline::Linear).await.expect("fills");
		assert_eq!(out.len(), 5);
		for (i, want) in [10.0, 10.0, 10.0, 10.0, 20.0].into_iter().enumerate() {
			assert!(approx(&out[i].value, want), "step point {i} should be {want}, got {}", out[i].value);
		}
	}

	#[tokio::test]
	async fn carries_the_last_sample_forward_past_the_end() {
		// After the final sample the value holds flat (no extrapolated slope, unlike
		// the linear baseline).
		let mut points = vec![pt(0, 3.0), pt(2, 7.0)];
		let out = ForwardFillAdapter::new().interpolate_range(&mut points, ts(2), ts(5), Resolution::Seconds, Spline::Linear).await.expect("fills");
		assert_eq!(out.len(), 4);
		assert!(out.iter().all(|p| approx(&p.value, 7.0)), "all values held at the last sample, 7");
	}

	#[tokio::test]
	async fn holds_the_first_sample_backward_before_the_range() {
		// The grid starts two seconds before the first sample; with no prior
		// observation the first value (5) is held backward so nothing is null.
		let mut points = vec![pt(2, 5.0), pt(4, 9.0)];
		let out = ForwardFillAdapter::new().interpolate_range(&mut points, ts(0), ts(4), Resolution::Seconds, Spline::Linear).await.expect("fills");
		assert_eq!(out.len(), 5);
		assert!(approx(&out[0].value, 5.0), "leading region holds the first value, got {}", out[0].value);
		assert!(approx(&out[1].value, 5.0));
		assert!(approx(&out[2].value, 5.0), "exact hit at t=2 is 5");
		assert!(approx(&out[3].value, 5.0), "t=3 still holds 5 until the next sample");
		assert!(approx(&out[4].value, 9.0), "steps to 9 at t=4");
	}

	#[tokio::test]
	async fn sorts_out_of_order_input_before_filling() {
		// Shuffled input (as a TSBS payload may arrive) must yield the same step
		// series as sorted input.
		let mut points = vec![pt(4, 20.0), pt(0, 10.0), pt(2, 15.0)];
		let out = ForwardFillAdapter::new().interpolate_range(&mut points, ts(0), ts(4), Resolution::Seconds, Spline::Linear).await.expect("fills");
		assert_eq!(out.len(), 5);
		assert!(approx(&out[1].value, 10.0), "t=1 holds the t=0 sample");
		assert!(approx(&out[2].value, 15.0), "t=2 steps to the middle sample");
		assert!(approx(&out[3].value, 15.0), "t=3 holds it");
	}

	#[tokio::test]
	async fn single_point_degenerates_to_a_constant() {
		let mut points = vec![pt(5, 42.0)];
		let out = ForwardFillAdapter::new().interpolate_range(&mut points, ts(0), ts(3), Resolution::Seconds, Spline::Linear).await.expect("fills");
		assert_eq!(out.len(), 4);
		assert!(out.iter().all(|p| approx(&p.value, 42.0)), "all values constant at 42");
	}

	#[tokio::test]
	async fn empty_input_is_rejected() {
		let mut points: Vec<Point> = Vec::new();
		let err = ForwardFillAdapter::new().interpolate_range(&mut points, ts(0), ts(3), Resolution::Seconds, Spline::Linear).await;
		assert!(err.is_err(), "an empty series cannot be filled");
	}

	#[tokio::test]
	async fn ignores_requested_spline_and_stays_a_step() {
		// Asking for Cubic must not change the (step) result: the baseline is
		// forward-fill by definition regardless of the requested method.
		let mut points = vec![pt(0, 1.0), pt(4, 2.0)];
		let out = ForwardFillAdapter::new().interpolate_range(&mut points, ts(0), ts(4), Resolution::Seconds, Spline::Cubic).await.expect("fills");
		for (i, want) in [1.0, 1.0, 1.0, 1.0, 2.0].into_iter().enumerate() {
			assert!(approx(&out[i].value, want), "cubic request still yields a step, point {i}");
		}
		assert_eq!(ForwardFillAdapter::new().name(), "baseline-forward-fill");
	}
}
