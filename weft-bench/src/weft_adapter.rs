//! WeftDB system adapter.
//!
//! Drives WeftDB's own interpolation engine (splimes' [`Interpolator`] on its default
//! `Backend::Auto`, which picks the serial, rayon or — once `splimes::calibrate` has run —
//! GPU backend by grid size) through the vendor-neutral [`SystemAdapter`] trait. This is
//! the reference adapter the competitor adapters are measured against.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use splimes::{Interpolator, Point, Resolution, Spline};

use crate::adapter::{AdapterInfo, SystemAdapter};

/// Adapter that benchmarks WeftDB's native interpolation path.
#[derive(Debug, Default, Clone, Copy)]
pub struct WeftAdapter;

impl WeftAdapter {
	/// Construct a WeftDB adapter.
	#[must_use]
	pub const fn new() -> Self {
		Self
	}
}

#[async_trait]
impl SystemAdapter for WeftAdapter {
	fn name(&self) -> &'static str {
		"weftdb"
	}

	fn adapter_info(&self) -> AdapterInfo {
		// Both facts are read off the implementation below, not asserted:
		// it drives splimes' `Interpolator` (a native engine, not an in-process
		// reimplementation) and passes the requested `spline` straight through
		// to `Interpolator::new`, unlike the baselines, which ignore it.
		AdapterInfo { interface_version: 2, engine: "splimes".to_string(), method: "native interpolation via splimes Interpolator (Backend::Auto)".to_string(), native_engine: true, honours_requested_spline: true }
	}

	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> anyhow::Result<Vec<Point>> {
		// splimes is synchronous and CPU-bound, so it runs on tokio's blocking pool, as it
		// does behind weft-server; the measured latency includes that hand-off. The blocking
		// task must own its input: the trait lets an adapter mutate the slice (the harness
		// hands every rep a fresh clone), so the values are moved out, not copied.
		let owned: Vec<Point> = points.iter_mut().map(|p| Point::new(p.timestamp, std::mem::take(&mut p.value))).collect();
		Ok(Interpolator::new(spline, resolution).run_async(owned, start, end).await?.into_points())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn report_names_the_engine_and_the_facts_the_code_shows() {
		let info = WeftAdapter::new().adapter_info();
		assert_eq!(info.interface_version, 2);
		assert_eq!(info.engine, "splimes");
		assert!(info.native_engine, "the adapter drives splimes, not an in-process reimplementation");
		assert!(info.honours_requested_spline, "the requested spline is passed to Interpolator::new");
	}
}
