//! Windowed threshold detectors: a live caller scans only the data after `since`
//! and finds the same crossings the full-history run finds in that window.

use std::str::FromStr;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use serial_test::serial;
use splimes::{Resolution, Spline};
use uuid::Uuid;
use weft_orchestration::{detect_threshold_crossing_down, detect_threshold_crossing_down_since, detect_threshold_crossing_up, detect_threshold_crossing_up_since};
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs}, Database, DatasetId, Event, InputMeasurement
};

fn starts(events: &[Event]) -> Vec<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
	let mut spans: Vec<_> = events.iter().flat_map(|e| e.manifestations().values().map(|m| (*m.start(), *m.end()))).collect();
	spans.sort();
	spans
}

#[serial]
#[tokio::test]
async fn windowed_runs_find_the_full_runs_crossings_in_the_window() -> Result<()> {
	let dir = tempfile::tempdir()?;
	std::env::set_var("WEFT_DATA_DIR", dir.path().to_str().unwrap());
	let db = Database::new(&format!("window_{}", Uuid::new_v4().simple())).await?;
	let subject = db.observe_subject("s").await?;
	let aspect = db.track_aspect(&subject.id(), "temp", &Resolution::Minutes, None).await?.id();

	// A sawtooth between 40 and 60 with a 10-minute period over two hours.
	let base = Utc.with_ymd_and_hms(2026, 11, 6, 0, 0, 0).unwrap();
	let rows: Vec<InputMeasurement> = (0..120).map(|m| InputMeasurement::new(base + Duration::minutes(m), BigDecimal::from(40 + (m % 10) * 2 + if m % 10 >= 5 { 10 } else { 0 }))).collect();
	db.batch_capture_measurements(aspect, DatasetId::new(), rows).await?;

	let threshold = BigDecimal::from_str("55")?;
	let since = base + Duration::minutes(61);
	let (res, spline) = (Resolution::Minutes, Spline::Linear);

	for (full, windowed) in [(detect_threshold_crossing_up(&db, &aspect, &res, &spline, threshold.clone(), "up").await?, detect_threshold_crossing_up_since(&db, &aspect, &res, &spline, threshold.clone(), "up", Some(since)).await?), (detect_threshold_crossing_down(&db, &aspect, &res, &spline, threshold.clone(), "down").await?, detect_threshold_crossing_down_since(&db, &aspect, &res, &spline, threshold.clone(), "down", Some(since)).await?)] {
		let in_window: Vec<_> = starts(&full).into_iter().filter(|(start, _)| *start >= since).collect();
		assert!(!in_window.is_empty(), "precondition: the window holds crossings");
		assert_eq!(starts(&windowed), in_window, "the windowed run finds exactly the full run's crossings in its window");
	}

	// `None` is the full run.
	assert_eq!(starts(&detect_threshold_crossing_up_since(&db, &aspect, &res, &spline, threshold.clone(), "up", None).await?), starts(&detect_threshold_crossing_up(&db, &aspect, &res, &spline, threshold, "up").await?));
	Ok(())
}
