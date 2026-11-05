use std::collections::HashSet;

use chrono::{DateTime, Utc};
use splimes::Resolution;

/// Represents a batch window defined by its start and end times
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BatchWindow {
	/// The start time of this batch window (inclusive)
	pub start: DateTime<Utc>,
	/// The end time of this batch window (exclusive)
	pub end: DateTime<Utc>,
}

impl BatchWindow {
	/// Create a new batch window
	#[must_use]
	pub const fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { start, end }
	}

	/// Check if a timestamp falls within this window
	#[must_use]
	pub fn contains(&self, timestamp: DateTime<Utc>) -> bool {
		timestamp >= self.start && timestamp < self.end
	}
}

/// Calculate which batch windows are affected by the given unbatched measurement timestamps.
///
/// Given a set of measurement timestamps and the resolution (step size), this function
/// determines which fixed-grid batch windows need to be created or updated.
///
/// Batch windows are aligned to the resolution step size, starting from the earliest
/// measurement time rounded down to the nearest step boundary.
///
/// # Arguments
/// * `unbatched_timestamps` - Timestamps of measurements that haven't been batched yet
/// * `resolution` - The time resolution determining the step size between batch windows
/// * `batch_size` - Number of points in each batch (determines window duration)
/// * `earliest_measurement` - The earliest measurement timestamp in the dataset (for window alignment)
///
/// # Returns
/// A set of unique batch windows that need to be created or updated
#[must_use]
pub fn calculate_affected_windows(unbatched_timestamps: &[DateTime<Utc>], resolution: &Resolution, batch_size: usize, earliest_measurement: DateTime<Utc>) -> HashSet<BatchWindow> {
	if unbatched_timestamps.is_empty() || batch_size == 0 {
		return HashSet::new();
	}

	let step = resolution.step();
	let step_millis = step.num_milliseconds();
	if step_millis <= 0 {
		return HashSet::new();
	}

	// Window duration is (batch_size - 1) * step since we have batch_size points
	// with step intervals between consecutive points
	let Ok(batch_len) = i64::try_from(batch_size) else {
		return HashSet::new();
	};
	let window_duration_millis = (batch_len - 1) * step_millis;

	// Align the earliest_measurement to the step boundary
	let base_millis = earliest_measurement.timestamp_millis();

	let mut affected_windows = HashSet::new();

	for &ts in unbatched_timestamps {
		let ts_millis = ts.timestamp_millis();

		// Find which windows could contain this timestamp
		// A timestamp at position T could be in any window where:
		//   window_start <= T < window_start + window_duration
		//
		// With sliding windows, the timestamp could be in multiple windows.
		// The earliest window that contains T starts at: T - window_duration + step
		// The latest window that contains T starts at: floor((T - base) / step) * step + base

		// Calculate the window indices that this timestamp affects
		// Window i starts at: base + i * step
		// Window i ends at: base + i * step + window_duration
		//
		// Timestamp T is in window i if: base + i * step <= T < base + i * step + window_duration
		// Which means: (T - base - window_duration) / step < i <= (T - base) / step

		let relative_ts = ts_millis - base_millis;

		// Latest window index that starts at or before this timestamp
		let latest_window_idx = relative_ts / step_millis;

		// Earliest window index that could contain this timestamp
		// A window of duration D can contain points up to D from its start
		let earliest_window_idx = (relative_ts - window_duration_millis) / step_millis + 1;
		let earliest_window_idx = earliest_window_idx.max(0);

		// Add all affected windows
		for window_idx in earliest_window_idx..=latest_window_idx {
			let window_start_millis = base_millis + window_idx * step_millis;
			let window_end_millis = window_start_millis + window_duration_millis;

			if let (Some(start), Some(end)) = (DateTime::from_timestamp_millis(window_start_millis), DateTime::from_timestamp_millis(window_end_millis)) {
				affected_windows.insert(BatchWindow::new(start, end));
			}
		}
	}

	affected_windows
}

/// Calculate which batch windows are affected by every timestamp in the
/// inclusive range `min_ts..=max_ts` (slice D-S20).
///
/// This is the union of the windows [`calculate_affected_windows`] computes for the range's two
/// ends, which is every window in between as well — the windows a
/// timestamp affects form one contiguous run of indices, and the runs of
/// two timestamps overlap or abut whenever the timestamps are within one
/// window duration of each other, which a change-log span's interior
/// timestamps always are relative to the nearer end's run extended across
/// the span. Formally: window `i` is affected by some timestamp in the
/// range iff `base + i * step <= max_ts` and
/// `base + i * step + window_duration >= min_ts`, and this function
/// returns exactly those windows without materialising the range's
/// timestamps (a change-log row names only the span).
///
/// This is what a seal-backed aspect's batch consumer uses: its ingest
/// commits record one change-log row per commit (span + row count), not
/// one queue entry per timestamp.
#[must_use]
pub fn calculate_affected_windows_for_range(min_ts: DateTime<Utc>, max_ts: DateTime<Utc>, resolution: &Resolution, batch_size: usize, earliest_measurement: DateTime<Utc>) -> HashSet<BatchWindow> {
	if batch_size == 0 || max_ts < min_ts {
		return HashSet::new();
	}
	let step = resolution.step();
	let step_millis = step.num_milliseconds();
	if step_millis <= 0 {
		return HashSet::new();
	}
	#[allow(clippy::cast_possible_wrap)]
	let Ok(batch_len) = i64::try_from(batch_size) else {
		return HashSet::new();
	};
	let window_duration_millis = (batch_len - 1) * step_millis;
	let base_millis = earliest_measurement.timestamp_millis();

	// Latest window index that starts at or before max_ts.
	let latest_window_idx = (max_ts.timestamp_millis() - base_millis) / step_millis;
	// Earliest window index whose end reaches min_ts.
	let earliest_window_idx = ((min_ts.timestamp_millis() - base_millis - window_duration_millis) / step_millis + 1).max(0);

	let mut affected_windows = HashSet::new();
	for window_idx in earliest_window_idx..=latest_window_idx {
		let window_start_millis = base_millis + window_idx * step_millis;
		let window_end_millis = window_start_millis + window_duration_millis;
		if let (Some(start), Some(end)) = (DateTime::from_timestamp_millis(window_start_millis), DateTime::from_timestamp_millis(window_end_millis)) {
			affected_windows.insert(BatchWindow::new(start, end));
		}
	}
	affected_windows
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone;

	use super::*;

	#[test]
	fn test_empty_timestamps() {
		let timestamps: Vec<DateTime<Utc>> = vec![];
		let resolution = Resolution::Minutes;
		let earliest = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

		let windows = calculate_affected_windows(&timestamps, &resolution, 10, earliest);
		assert!(windows.is_empty());
	}

	#[test]
	fn test_single_timestamp() {
		let earliest = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
		let timestamps = vec![Utc.with_ymd_and_hms(2024, 1, 1, 0, 5, 0).unwrap()];
		let resolution = Resolution::Minutes;
		let batch_size = 3; // 3 points = 2 minute window

		let windows = calculate_affected_windows(&timestamps, &resolution, batch_size, earliest);

		// A timestamp at minute 5 with batch_size=3 could be in windows starting at:
		// minute 3 (covers 3-5), minute 4 (covers 4-6), minute 5 (covers 5-7)
		assert!(!windows.is_empty());
	}

	#[test]
	fn test_range_windows_cover_every_timestamp_in_the_range() {
		let earliest = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
		let resolution = Resolution::Minutes;
		let batch_size = 3;
		// Every minute in the span, as the per-timestamp queue would name them.
		let timestamps: Vec<DateTime<Utc>> = (3..=9).map(|m| Utc.with_ymd_and_hms(2024, 1, 1, 0, m, 0).unwrap()).collect();
		let per_timestamp = calculate_affected_windows(&timestamps, &resolution, batch_size, earliest);
		let per_range = calculate_affected_windows_for_range(timestamps[0], *timestamps.last().unwrap(), &resolution, batch_size, earliest);
		assert_eq!(per_range, per_timestamp, "the span's windows are the union of its timestamps' windows");
	}

	#[test]
	fn test_batch_window_contains() {
		let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2024, 1, 1, 0, 5, 0).unwrap();
		let window = BatchWindow::new(start, end);

		// Timestamp at start is included
		assert!(window.contains(start));

		// Timestamp in middle is included
		let middle = Utc.with_ymd_and_hms(2024, 1, 1, 0, 2, 30).unwrap();
		assert!(window.contains(middle));

		// Timestamp at end is NOT included (exclusive end)
		assert!(!window.contains(end));

		// Timestamp before start is not included
		let before = Utc.with_ymd_and_hms(2023, 12, 31, 23, 59, 59).unwrap();
		assert!(!window.contains(before));
	}
}
