//! Vendor-neutral system-adapter abstraction.
//!
//! Every benchmarked system — WeftDB today, and `ClickHouse` / `InfluxDB 3` /
//! `QuestDB` / `TimescaleDB` / `DuckDB` as they are added — is driven through
//! this single trait.
//! Keeping the abstraction vendor-neutral (and the concrete adapters as separate
//! modules/crates) mirrors the roadmap's connector hard-constraint: no
//! vendor-specific coupling leaks into the harness core.
//!
//! The trait currently models the flagship interpolation workload. Additional
//! workload methods (range fetch, downsample, compression, …) will be added as
//! their benchmarks come online; each gets a default so adapters opt in.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use splimes::{Point, Resolution, Spline};

/// The reconstruction grid every adapter fills: `start, start + step, …` up to and
/// including the last instant not after `end`, at `resolution`'s fixed step (empty when
/// `start > end`).
///
/// This is splimes' own output grid (what `splimes::Interpolation::timestamps` returns),
/// spelled out for the adapters that compute their own values and for the harness's
/// expected-size check; splimes 1.0 dropped the standalone `generate_target_times`.
#[must_use]
pub fn grid_timestamps(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
	let step = resolution.step();
	std::iter::successors(Some(start), |t| t.checked_add_signed(step)).take_while(|t| *t <= end).collect()
}

/// A system under benchmark.
///
/// Implementations must be cheap to construct and safe to call repeatedly; the
/// harness times `interpolate_range` across many reps.
#[async_trait]
pub trait SystemAdapter: Send + Sync {
	/// Short, stable identifier recorded in results (e.g. `weftdb`).
	fn name(&self) -> &'static str;

	/// Reconstruct a dense regular grid over `[start, end)` at `resolution`
	/// using `spline`, given the (possibly irregular, gap-containing) input
	/// `points`.
	///
	/// The slice is `&mut` because some engines sort/normalize in place (or, like
	/// the WeftDB adapter, move the values out to a blocking task); the harness
	/// always hands over a fresh clone per rep so mutation is safe.
	///
	/// # Errors
	///
	/// Returns an error if the adapter cannot produce an interpolated series
	/// (e.g. insufficient input, backend failure).
	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> anyhow::Result<Vec<Point>>;
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use chrono::TimeZone;
	use splimes::Interpolator;

	use super::*;

	fn ts(secs: i64) -> DateTime<Utc> {
		Utc.timestamp_opt(secs, 0).single().expect("valid instant")
	}

	#[test]
	fn grid_is_inclusive_anchored_at_start_and_empty_when_reversed() {
		assert_eq!(grid_timestamps(ts(0), ts(10), Resolution::Seconds), (0..=10).map(ts).collect::<Vec<_>>());
		// `end` off the grid: the last point is the last step not after it.
		assert_eq!(grid_timestamps(ts(5), ts(130), Resolution::Minutes), vec![ts(5), ts(65), ts(125)]);
		assert_eq!(grid_timestamps(ts(7), ts(7), Resolution::Days), vec![ts(7)]);
		assert!(grid_timestamps(ts(8), ts(7), Resolution::Seconds).is_empty());
	}

	#[test]
	fn grid_is_the_one_splimes_fills() {
		// The adapters' expected-size check and the baselines rely on this being exactly
		// splimes' own output grid, for every resolution.
		let points = [Point::new(ts(0), BigDecimal::from(1)), Point::new(ts(90_000), BigDecimal::from(2))];
		for &resolution in &[Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days, Resolution::Weeks, Resolution::Months, Resolution::Years] {
			// A few hundred steps, starting and ending off the epoch grid.
			let step = resolution.step().num_seconds();
			let (start, end) = (ts(-3 * step - 17), ts(400 * step + 5));
			let series = Interpolator::new(Spline::Linear, resolution).run(&points, start, end).expect("interpolates");
			assert_eq!(series.timestamps(), grid_timestamps(start, end, resolution).as_slice(), "{resolution:?}");
		}
	}
}
