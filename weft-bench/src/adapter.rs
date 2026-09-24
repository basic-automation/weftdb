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
	/// The slice is `&mut` because some engines sort/normalize in place; the
	/// harness always hands over a fresh clone per rep so mutation is safe.
	///
	/// # Errors
	///
	/// Returns an error if the adapter cannot produce an interpolated series
	/// (e.g. insufficient input, backend failure).
	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> anyhow::Result<Vec<Point>>;
}
