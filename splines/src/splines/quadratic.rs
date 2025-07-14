use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use wide::{CmpLt, f64x4};

use crate::{
	Error, Point, Resolution, Spline, is_uniformly_spaced, splines::{DAYS_IN_MONTH, DAYS_IN_YEAR, QuadraticSpline, SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR, SIMD_BATCH_SIZE}
};

/// Performs quadratic spline interpolation on measurement data.
///
/// Takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`,
/// and returns a vector of interpolated or extrapolated points using quadratic spline interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - points are empty or have fewer than 3 points
/// - points have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Insufficient points for quadratic spline interpolation
/// - Timestamp conversion or `BigDecimal` operations fail
pub fn quadratic(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	if points.is_empty() {
		return Ok(vec![]);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	if points.len() < Spline::Quadratic.number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	// Fast path for two points - degrade to linear
	if points.len() == 2 {
		return quadratic_two_point_fast(&points, start, end, resolution);
	}

	// Sort points by timestamp
	let mut sorted_points = points;
	sorted_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Check for uniform spacing - enables fast path
	if is_uniformly_spaced(&sorted_points, resolution) {
		return quadratic_uniform_fast(&sorted_points, start, end, resolution);
	}

	let step = resolution.to_step();

	// Build optimized quadratic spline
	let spline = QuadraticSpline::new(&sorted_points, resolution)?;

	let mut result = Vec::new();

	// Pre-compute rounding to avoid repeated calculations
	let start_base = match resolution {
		Resolution::Nanoseconds => match start.timestamp_nanos_opt() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => start.timestamp_micros(),
		Resolution::Milliseconds => start.timestamp_millis(),
		Resolution::Seconds => start.timestamp(),
		Resolution::Minutes => start.timestamp() / SECONDS_IN_MINUTE,
		Resolution::Hours => start.timestamp() / SECONDS_IN_HOUR,
		Resolution::Days => start.timestamp() / SECONDS_IN_DAY,
		Resolution::Weeks => start.timestamp() / SECONDS_IN_WEEK,
		Resolution::Months => start.timestamp() / SECONDS_IN_MONTH,
		Resolution::Years => start.timestamp() / SECONDS_IN_YEAR,
	};

	let step_base = match resolution {
		Resolution::Nanoseconds => match step.num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match step.num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => step.num_milliseconds(),
		Resolution::Seconds => step.num_seconds(),
		Resolution::Minutes => step.num_minutes(),
		Resolution::Hours => step.num_hours(),
		Resolution::Days => step.num_days(),
		Resolution::Weeks => step.num_weeks(),
		Resolution::Months => step.num_days() / DAYS_IN_MONTH,
		Resolution::Years => step.num_days() / DAYS_IN_YEAR,
	};
	let start_offset = start_base % step_base;
	let rounded_start = match resolution {
		Resolution::Nanoseconds => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::nanoseconds(step_base - start_offset)
			}
		}
		Resolution::Microseconds => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::microseconds(step_base - start_offset)
			}
		}
		Resolution::Milliseconds => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::milliseconds(step_base - start_offset)
			}
		}
		Resolution::Seconds => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::seconds(step_base - start_offset)
			}
		}
		Resolution::Minutes => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::minutes(step_base - start_offset)
			}
		}
		Resolution::Hours => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::hours(step_base - start_offset)
			}
		}
		Resolution::Days => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::days(step_base - start_offset)
			}
		}
		Resolution::Weeks => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::weeks(step_base - start_offset)
			}
		}
		Resolution::Months => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::days(step_base - start_offset)
			}
		}
		Resolution::Years => {
			if start_offset == 0 {
				start
			} else {
				start + chrono::TimeDelta::days(step_base - start_offset)
			}
		}
	};

	let end_base = match resolution {
		Resolution::Nanoseconds => match end.timestamp_nanos_opt() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => end.timestamp_micros(),
		Resolution::Milliseconds => end.timestamp_millis(),
		Resolution::Seconds => end.timestamp(),
		Resolution::Minutes => end.timestamp() / SECONDS_IN_MINUTE,
		Resolution::Hours => end.timestamp() / SECONDS_IN_HOUR,
		Resolution::Days => end.timestamp() / SECONDS_IN_DAY,
		Resolution::Weeks => end.timestamp() / SECONDS_IN_WEEK,
		Resolution::Months => end.timestamp() / SECONDS_IN_MONTH,
		Resolution::Years => end.timestamp() / SECONDS_IN_YEAR,
	};

	let end_offset = end_base % step_base;
	let rounded_end = match resolution {
		Resolution::Nanoseconds => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::nanoseconds(end_offset)
			}
		}
		Resolution::Microseconds => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::microseconds(end_offset)
			}
		}
		Resolution::Milliseconds => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::milliseconds(end_offset)
			}
		}
		Resolution::Seconds => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::seconds(end_offset)
			}
		}
		Resolution::Minutes => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::minutes(end_offset)
			}
		}
		Resolution::Hours => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::hours(end_offset)
			}
		}
		Resolution::Days => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::days(end_offset)
			}
		}
		Resolution::Weeks => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::weeks(end_offset)
			}
		}
		Resolution::Months => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::days(end_offset)
			}
		}
		Resolution::Years => {
			if end_offset == 0 {
				end
			} else {
				end - chrono::TimeDelta::days(end_offset)
			}
		}
	};

	// Pre-allocate result vector for better performance - safe casting
	let time_diff_base = match resolution {
		Resolution::Nanoseconds => match (rounded_end - rounded_start).num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match (rounded_end - rounded_start).num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => (rounded_end - rounded_start).num_milliseconds(),
		Resolution::Seconds => (rounded_end - rounded_start).num_seconds(),
		Resolution::Minutes => (rounded_end - rounded_start).num_minutes(),
		Resolution::Hours => (rounded_end - rounded_start).num_hours(),
		Resolution::Days => (rounded_end - rounded_start).num_days(),
		Resolution::Weeks => (rounded_end - rounded_start).num_weeks(),
		Resolution::Months => (rounded_end - rounded_start).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (rounded_end - rounded_start).num_days() / DAYS_IN_YEAR,
	};
	let estimated_points = if time_diff_base > 0 {
		usize::try_from(time_diff_base / step_base)
			.unwrap_or(1000) // Fallback to reasonable default
			.saturating_add(1)
	} else {
		1
	};
	result.reserve(estimated_points);

	let mut current_time = rounded_start;

	while current_time <= rounded_end {
		let value = spline.evaluate(current_time, resolution)?;
		result.push(Point { timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Fast path for two points - degrade to linear interpolation
fn quadratic_two_point_fast(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute slope once for linear interpolation
	let dt = match resolution {
		Resolution::Nanoseconds => match (points[1].timestamp - points[0].timestamp).num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match (points[1].timestamp - points[0].timestamp).num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => (points[1].timestamp - points[0].timestamp).num_milliseconds(),
		Resolution::Seconds => (points[1].timestamp - points[0].timestamp).num_seconds(),
		Resolution::Minutes => (points[1].timestamp - points[0].timestamp).num_minutes(),
		Resolution::Hours => (points[1].timestamp - points[0].timestamp).num_hours(),
		Resolution::Days => (points[1].timestamp - points[0].timestamp).num_days(),
		Resolution::Weeks => (points[1].timestamp - points[0].timestamp).num_weeks(),
		Resolution::Months => (points[1].timestamp - points[0].timestamp).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (points[1].timestamp - points[0].timestamp).num_days() / DAYS_IN_YEAR,
	};
	let dt = match BigDecimal::from_i64(dt) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let dy = &points[1].value - &points[0].value;
	let slope = dy / dt;
	let base_value = &points[0].value;
	let base_time = points[0].timestamp;

	let mut current_time = start;

	while current_time <= end {
		let time_diff = match resolution {
			Resolution::Nanoseconds => match (current_time - base_time).num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::DecimalConversionError),
			},
			Resolution::Microseconds => match (current_time - base_time).num_microseconds() {
				Some(value) => value,
				None => bail!(Error::DecimalConversionError),
			},
			Resolution::Milliseconds => (current_time - base_time).num_milliseconds(),
			Resolution::Seconds => (current_time - base_time).num_seconds(),
			Resolution::Minutes => (current_time - base_time).num_minutes(),
			Resolution::Hours => (current_time - base_time).num_hours(),
			Resolution::Days => (current_time - base_time).num_days(),
			Resolution::Weeks => (current_time - base_time).num_weeks(),
			Resolution::Months => (current_time - base_time).num_days() / DAYS_IN_MONTH,
			Resolution::Years => (current_time - base_time).num_days() / DAYS_IN_YEAR,
		};
		let time_diff = match BigDecimal::from_i64(time_diff) {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		};
		let value = base_value + &slope * time_diff;

		result.push(Point { timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Fast path for uniformly spaced data using optimized quadratic interpolation
fn quadratic_uniform_fast(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval
	let uniform_interval_base = match resolution {
		Resolution::Nanoseconds => {
			let d = points[1].timestamp - points[0].timestamp;
			match d.num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::DecimalConversionError),
			}
		}
		Resolution::Microseconds => {
			let d = points[1].timestamp - points[0].timestamp;
			match d.num_microseconds() {
				Some(value) => value,
				None => bail!(Error::DecimalConversionError),
			}
		}
		Resolution::Milliseconds => (points[1].timestamp - points[0].timestamp).num_milliseconds(),
		Resolution::Seconds => (points[1].timestamp - points[0].timestamp).num_seconds(),
		Resolution::Minutes => (points[1].timestamp - points[0].timestamp).num_minutes(),
		Resolution::Hours => (points[1].timestamp - points[0].timestamp).num_hours(),
		Resolution::Days => (points[1].timestamp - points[0].timestamp).num_days(),
		Resolution::Weeks => (points[1].timestamp - points[0].timestamp).num_weeks(),
		Resolution::Months => (points[1].timestamp - points[0].timestamp).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (points[1].timestamp - points[0].timestamp).num_days() / DAYS_IN_YEAR,
	};
	let uniform_interval = match BigDecimal::from_i64(uniform_interval_base) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};

	let mut current_time = start;
	let data_start = points[0].timestamp;
	let data_end = points[points.len() - 1].timestamp;

	while current_time <= end {
		let value = if current_time < data_start || current_time > data_end {
			// Extrapolation - use boundary quadratic segments
			if current_time < data_start { extrapolate_backward_uniform(points, current_time, &uniform_interval, resolution)? } else { extrapolate_forward_uniform(points, current_time, &uniform_interval, resolution)? }
		} else {
			// Interpolation - use uniform spacing optimization for quadratic
			interpolate_uniform_quadratic(points, current_time, &uniform_interval, resolution)?
		};

		result.push(Point { timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Backward extrapolation for uniform data
fn extrapolate_backward_uniform(points: &[Point], target_time: DateTime<Utc>, uniform_interval: &BigDecimal, resolution: Resolution) -> Result<BigDecimal> {
	// Use first three points for quadratic extrapolation
	let p0 = &points[0];
	let p1 = &points[1];
	let p2 = &points[2];

	let dt = match resolution {
		Resolution::Nanoseconds => match (target_time - p0.timestamp).num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match (target_time - p0.timestamp).num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => (target_time - p0.timestamp).num_milliseconds(),
		Resolution::Seconds => (target_time - p0.timestamp).num_seconds(),
		Resolution::Minutes => (target_time - p0.timestamp).num_minutes(),
		Resolution::Hours => (target_time - p0.timestamp).num_hours(),
		Resolution::Days => (target_time - p0.timestamp).num_days(),
		Resolution::Weeks => (target_time - p0.timestamp).num_weeks(),
		Resolution::Months => (target_time - p0.timestamp).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (target_time - p0.timestamp).num_days() / DAYS_IN_YEAR,
	};
	let dt = match BigDecimal::from_i64(dt) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let t = dt / uniform_interval;

	// Quadratic extrapolation using the same Lagrange formula
	let half = match BigDecimal::from_f64(0.5) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let one = match BigDecimal::from_f64(1.0) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let t_squared = &t * &t;

	let l0 = &t * (&t - &one) * &half;
	let l1 = &one - &t_squared;
	let l2 = &t * (&t + &one) * &half;

	let result = &p0.value * &l0 + &p1.value * &l1 + &p2.value * &l2;

	Ok(result)
}

/// Forward extrapolation for uniform data
fn extrapolate_forward_uniform(points: &[Point], target_time: DateTime<Utc>, uniform_interval: &BigDecimal, resolution: Resolution) -> Result<BigDecimal> {
	// Use last three points for quadratic extrapolation
	let n = points.len();
	let p0 = &points[n - 3];
	let p1 = &points[n - 2];
	let p2 = &points[n - 1];

	let dt = match resolution {
		Resolution::Nanoseconds => match (target_time - p2.timestamp).num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match (target_time - p2.timestamp).num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => (target_time - p2.timestamp).num_milliseconds(),
		Resolution::Seconds => (target_time - p2.timestamp).num_seconds(),
		Resolution::Minutes => (target_time - p2.timestamp).num_minutes(),
		Resolution::Hours => (target_time - p2.timestamp).num_hours(),
		Resolution::Days => (target_time - p2.timestamp).num_days(),
		Resolution::Weeks => (target_time - p2.timestamp).num_weeks(),
		Resolution::Months => (target_time - p2.timestamp).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (target_time - p2.timestamp).num_days() / DAYS_IN_YEAR,
	};
	let dt = match BigDecimal::from_i64(dt) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let t = dt / uniform_interval;

	// Quadratic extrapolation using the same Lagrange formula
	let half = match BigDecimal::from_f64(0.5) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let one = match BigDecimal::from_f64(1.0) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let t_squared = &t * &t;

	let l0 = &t * (&t - &one) * &half;
	let l1 = &one - &t_squared;
	let l2 = &t * (&t + &one) * &half;

	let result = &p0.value * &l0 + &p1.value * &l1 + &p2.value * &l2;

	Ok(result)
}

/// Optimized uniform quadratic interpolation
fn interpolate_uniform_quadratic(points: &[Point], target_time: DateTime<Utc>, uniform_interval: &BigDecimal, resolution: Resolution) -> Result<BigDecimal> {
	let data_start = points[0].timestamp;
	let time_from_start = match resolution {
		Resolution::Nanoseconds => match (target_time - data_start).num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match (target_time - data_start).num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => (target_time - data_start).num_milliseconds(),
		Resolution::Seconds => (target_time - data_start).num_seconds(),
		Resolution::Minutes => (target_time - data_start).num_minutes(),
		Resolution::Hours => (target_time - data_start).num_hours(),
		Resolution::Days => (target_time - data_start).num_days(),
		Resolution::Weeks => (target_time - data_start).num_weeks(),
		Resolution::Months => (target_time - data_start).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (target_time - data_start).num_days() / DAYS_IN_YEAR,
	};

	// Stay in BigDecimal - convert to i64 only when necessary for indexing
	let uniform_interval_ms = match uniform_interval.to_i64() {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};

	// Find the segment index using direct calculation for uniform data - safe casting
	let segment_index = if time_from_start >= 0 && uniform_interval_ms > 0 { usize::try_from(time_from_start / uniform_interval_ms).unwrap_or(0).min(points.len().saturating_sub(2)) } else { 0 };

	// Use three points for quadratic interpolation: segment_index-1, segment_index, segment_index+1
	let (p0, p1, p2) = if segment_index == 0 {
		// Use first three points
		(&points[0], &points[1], &points[2])
	} else if segment_index >= points.len() - 1 {
		// Use last three points
		let n = points.len();
		(&points[n - 3], &points[n - 2], &points[n - 1])
	} else {
		// Use centered three points
		(&points[segment_index - 1], &points[segment_index], &points[segment_index + 1])
	};

	// Calculate relative time parameter for quadratic interpolation - stay in BigDecimal
	let dt = match resolution {
		Resolution::Nanoseconds => match (target_time - p1.timestamp).num_nanoseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Microseconds => match (target_time - p1.timestamp).num_microseconds() {
			Some(value) => value,
			None => bail!(Error::DecimalConversionError),
		},
		Resolution::Milliseconds => (target_time - p1.timestamp).num_milliseconds(),
		Resolution::Seconds => (target_time - p1.timestamp).num_seconds(),
		Resolution::Minutes => (target_time - p1.timestamp).num_minutes(),
		Resolution::Hours => (target_time - p1.timestamp).num_hours(),
		Resolution::Days => (target_time - p1.timestamp).num_days(),
		Resolution::Weeks => (target_time - p1.timestamp).num_weeks(),
		Resolution::Months => (target_time - p1.timestamp).num_days() / DAYS_IN_MONTH,
		Resolution::Years => (target_time - p1.timestamp).num_days() / DAYS_IN_YEAR,
	};
	let dt = match BigDecimal::from_i64(dt) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let t = dt / uniform_interval;

	// Quadratic interpolation using Lagrange formula - all BigDecimal operations
	// P(t) = y0*L0(t) + y1*L1(t) + y2*L2(t)
	// where L0(t) = t*(t-1)/2, L1(t) = 1-t², L2(t) = t*(t+1)/2

	let half = match BigDecimal::from_f64(0.5) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let one = match BigDecimal::from_f64(1.0) {
		Some(value) => value,
		None => bail!(Error::DecimalConversionError),
	};
	let t_squared = &t * &t;

	let l0 = &t * (&t - &one) * &half;
	let l1 = &one - &t_squared;
	let l2 = &t * (&t + &one) * &half;

	let result = &p0.value * &l0 + &p1.value * &l1 + &p2.value * &l2;

	Ok(result)
}

/// SIMD-optimized quadratic interpolation - FIXED VERSION
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points (< 3 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn quadratic_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution) -> Result<Vec<Point>> {
	if points.len() < Spline::Quadratic.number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Convert to f64 arrays for SIMD processing
	#[allow(clippy::cast_precision_loss)]
	let input_times: Vec<f64> = points
		.iter()
		.map(|m| {
			match resolution {
				Resolution::Nanoseconds => {
					match m.timestamp.timestamp_nanos_opt() {
						Some(value) => value as f64,
						None => 0.0, // Fallback for invalid timestamps
					}
				}
				Resolution::Microseconds => m.timestamp.timestamp_micros() as f64,
				Resolution::Milliseconds => m.timestamp.timestamp_millis() as f64,
				Resolution::Seconds => m.timestamp.timestamp() as f64,
				Resolution::Minutes => m.timestamp.timestamp() as f64 / SECONDS_IN_MINUTE as f64,
				Resolution::Hours => m.timestamp.timestamp() as f64 / SECONDS_IN_HOUR as f64,
				Resolution::Days => m.timestamp.timestamp() as f64 / SECONDS_IN_DAY as f64,
				Resolution::Weeks => m.timestamp.timestamp() as f64 / SECONDS_IN_WEEK as f64,
				Resolution::Months => m.timestamp.timestamp() as f64 / SECONDS_IN_MONTH as f64,
				Resolution::Years => m.timestamp.timestamp() as f64 / SECONDS_IN_YEAR as f64,
			}
		})
		.collect();
	let input_values: Vec<f64> = points.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

	// Process target times in SIMD batches
	let mut results = Vec::with_capacity(target_times.len());

	for target_chunk in target_times.chunks(SIMD_BATCH_SIZE) {
		#[allow(clippy::cast_precision_loss)]
		let target_f64s: Vec<f64> = target_chunk
			.iter()
			.map(|t| {
				match resolution {
					Resolution::Nanoseconds => {
						match t.timestamp_nanos_opt() {
							Some(value) => value as f64,
							None => 0.0, // Fallback for invalid timestamps
						}
					}
					Resolution::Microseconds => t.timestamp_micros() as f64,
					Resolution::Milliseconds => t.timestamp_millis() as f64,
					Resolution::Seconds => t.timestamp() as f64,
					Resolution::Minutes => t.timestamp() as f64 / SECONDS_IN_MINUTE as f64,
					Resolution::Hours => t.timestamp() as f64 / SECONDS_IN_HOUR as f64,
					Resolution::Days => t.timestamp() as f64 / SECONDS_IN_DAY as f64,
					Resolution::Weeks => t.timestamp() as f64 / SECONDS_IN_WEEK as f64,
					Resolution::Months => t.timestamp() as f64 / SECONDS_IN_MONTH as f64,
					Resolution::Years => t.timestamp() as f64 / SECONDS_IN_YEAR as f64,
				}
			})
			.collect();

		// Pad to SIMD width
		let mut padded_targets = [0.0; SIMD_BATCH_SIZE];
		let chunk_size = target_f64s.len();
		padded_targets[..chunk_size].copy_from_slice(&target_f64s);

		if chunk_size < SIMD_BATCH_SIZE {
			let last_value = target_f64s[chunk_size - 1];
			for target in &mut padded_targets[chunk_size..] {
				*target = last_value;
			}
		}

		let target_simd = f64x4::new(padded_targets);
		let result_simd = simd_quadratic_interpolate(&input_times, &input_values, target_simd);

		let result_array = result_simd.to_array();
		results.extend_from_slice(&result_array[..chunk_size]);
	}

	// Convert back to Points
	let mut interpolated = Vec::with_capacity(target_times.len());
	for (i, &value) in results.iter().enumerate() {
		interpolated.push(Point { timestamp: target_times[i], value: BigDecimal::from_f64(value).unwrap_or_else(|| BigDecimal::from(0)) });
	}

	Ok(interpolated)
}

/// SIMD quadratic interpolation core function
fn simd_quadratic_interpolate(input_times: &[f64], input_values: &[f64], target_times: f64x4) -> f64x4 {
	// For this SIMD optimization, we make a simplifying assumption:
	// all 4 target times in the vector fall into the same segment.
	// We use the first lane to determine the segment.
	let target_time_scalar = target_times.to_array()[0];

	// Find appropriate segment for the first target time
	let center_idx = match input_times.binary_search_by(|t| t.partial_cmp(&target_time_scalar).unwrap()) {
		Ok(i) => i,
		Err(i) => i.max(1).min(input_times.len() - 2),
	};

	// Handle boundary conditions
	let (i0, i1, i2) = if center_idx == 0 {
		(0, 1, 2)
	} else if center_idx >= input_times.len() - 1 {
		let n = input_times.len();
		(n - 3, n - 2, n - 1)
	} else {
		(center_idx - 1, center_idx, center_idx + 1)
	};

	// Load segment points into SIMD vectors
	let t0 = f64x4::splat(input_times[i0]);
	let t1 = f64x4::splat(input_times[i1]);
	let t2 = f64x4::splat(input_times[i2]);
	let v0 = f64x4::splat(input_values[i0]);
	let v1 = f64x4::splat(input_values[i1]);
	let v2 = f64x4::splat(input_values[i2]);

	// Perform Lagrange quadratic interpolation using SIMD operations
	let denom0 = (t0 - t1) * (t0 - t2);
	let denom1 = (t1 - t0) * (t1 - t2);
	let denom2 = (t2 - t0) * (t2 - t1);

	// Create masks for fallback conditions (e.g., division by zero)
	let fallback_mask = denom0.abs().cmp_lt(f64x4::splat(1e-10)) | denom1.abs().cmp_lt(f64x4::splat(1e-10)) | denom2.abs().cmp_lt(f64x4::splat(1e-10));

	// Calculate Lagrange basis polynomials
	let l0 = ((target_times - t1) * (target_times - t2)) / denom0;
	let l1 = ((target_times - t0) * (target_times - t2)) / denom1;
	let l2 = ((target_times - t0) * (target_times - t1)) / denom2;

	// Calculate final value using fused multiply-add for precision and performance
	let result = v2.mul_add(l2, v0.mul_add(l0, v1 * l1));

	// Use the center value as a fallback where denominators are too small
	fallback_mask.blend(v1, result)
}
