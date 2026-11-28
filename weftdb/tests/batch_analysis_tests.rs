#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use splimes::Resolution;
use weftdb::{AspectId, Batch, BatchedMeasurement, Batches, DatabaseInfo, Point};

/// Verify that `process_batches` populates `analysis` and `relative` on every
/// measurement, matching the behaviour `Batch::process` provides.
#[test]
fn process_batches_populates_relative_like_process() -> Result<()> {
	let aspect_id = AspectId::new();
	let info = DatabaseInfo::new("test".into(), "/tmp".into());
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

	let points: Vec<BatchedMeasurement> = (0..5)
		.map(|i| {
			BatchedMeasurement::new(Point::new(
				base + Duration::minutes(i),
				BigDecimal::from((i * 7) % 11),
			))
		})
		.collect();

	let batch = Batch::new(5, points, Resolution::Minutes, aspect_id, info);
	let mut batches = Batches::new_from_vec(vec![batch]);
	batches.process_batches()?;

	let processed = batches.get(0).context("expected exactly one batch")?;
	for measurement in processed.measurements() {
		let analysis =
			measurement.analysis().context("analysis should be Some after process_batches")?;
		assert!(
			analysis.relative().is_some(),
			"relative should be Some after process_batches"
		);
	}

	Ok(())
}

/// For identical input, `process_batches` must leave each measurement in the
/// same state as `Batch::process` — compare the `Analysis` values directly.
#[test]
fn process_batches_relative_parity_with_process() -> Result<()> {
	let aspect_id = AspectId::new();
	let info = DatabaseInfo::new("test".into(), "/tmp".into());
	let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

	let points1: Vec<BatchedMeasurement> = (0..5)
		.map(|i| {
			BatchedMeasurement::new(Point::new(
				base + Duration::minutes(i),
				BigDecimal::from((i * 7) % 11),
			))
		})
		.collect();
	let points2: Vec<BatchedMeasurement> = (0..5)
		.map(|i| {
			BatchedMeasurement::new(Point::new(
				base + Duration::minutes(i),
				BigDecimal::from((i * 7) % 11),
			))
		})
		.collect();

	let batch1 = Batch::new(5, points1, Resolution::Minutes, aspect_id, info.clone());
	let mut batch2 = Batch::new(5, points2, Resolution::Minutes, aspect_id, info);

	let mut batches = Batches::new_from_vec(vec![batch1]);
	batches.process_batches()?;
	batch2.process()?;

	let processed = batches.get(0).context("expected exactly one batch")?;
	for (i, (m1, m2)) in processed
		.measurements()
		.iter()
		.zip(batch2.measurements().iter())
		.enumerate()
	{
		assert_eq!(m1.analysis(), m2.analysis(), "analysis mismatch at index {i}");
	}

	Ok(())
}