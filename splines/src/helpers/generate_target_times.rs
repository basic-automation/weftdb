use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sysinfo::System;

use crate::Resolution;

// Iterator for paginated target times with dynamic batch size
pub struct TargetTimesIterator {
	current: DateTime<Utc>,
	end: DateTime<Utc>,
	step: Duration,
	system: System,
	resolution: Resolution,
}

impl TargetTimesIterator {
	pub fn new(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Self {
		let step = resolution.to_step();

		TargetTimesIterator { current: start, end, step, system: System::new_all(), resolution }
	}

	// Estimate the total number of target times without generating them
	pub fn estimate_len(&self) -> Result<usize> {
		if self.current > self.end {
			return Ok(0);
		}

		let time_diff = self.resolution.difference(&self.end, &self.current)?;
		let step_base = self.resolution.to_step_base()?;
		Ok(((time_diff + step_base - 1) / step_base) as usize)
	}
}

impl Iterator for TargetTimesIterator {
	type Item = Vec<DateTime<Utc>>;

	fn next(&mut self) -> Option<Self::Item> {
		if self.current > self.end {
			return None;
		}

		self.system.refresh_memory();
		let available_memory = self.system.free_memory().max(self.system.available_memory()); // In bytes
		let memory_threshold = (self.system.total_memory() as f64 * 0.8) as u64; // 80% memory threshold
		let point_size = std::mem::size_of::<DateTime<Utc>>() as u64; // ~16 bytes

		// Adjust batch size: scale down if available memory is low
		let batch_size = {
			let max_memory = available_memory.min(memory_threshold);
			let max_points = max_memory / point_size;
			(max_points / 2).max(100) as usize // Ensure minimum batch size
		};

		let mut batch = Vec::with_capacity(batch_size);
		for _ in 0..batch_size {
			if self.current > self.end {
				break;
			}
			batch.push(self.current);
			self.current += self.step;
		}

		if batch.is_empty() { None } else { Some(batch) }
	}
}

// Wrapper function to maintain original signature
pub fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
	TargetTimesIterator::new(start, end, resolution).flat_map(|batch| batch).collect()
}
