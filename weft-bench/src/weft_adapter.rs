//! WeftDB system adapter.
//!
//! Drives WeftDB's own interpolation engine (`splimes::auto_interpolate`, which
//! already selects CPU / SIMD / parallel / GPU strategies internally) through
//! the vendor-neutral [`SystemAdapter`] trait. This is the reference adapter the
//! competitor adapters are measured against.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use splimes::{Point, Resolution, Spline};

use crate::adapter::SystemAdapter;

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

	async fn interpolate_range(&self, points: &mut [Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> anyhow::Result<Vec<Point>> {
		splimes::auto_interpolate(points, start, end, resolution, spline).await
	}
}
