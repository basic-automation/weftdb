use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use wide::f64x4;

use super::{LinearSpline, SIMD_BATCH_SIZE};
use crate::{
	Error, Point, Resolution, Spline, is_uniformly_spaced, splines::{DAYS_IN_MONTH, DAYS_IN_YEAR, SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR}
};

/// Performs linear interpolation on points data.
///
/// Takes a vector of `points`, a start date/time, an end date/time, a `Resolution`,
/// and returns a vector of interpolated or extrapolated points using linear interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - points are empty or have fewer than 2 points
/// - points have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Timestamp conversion or `BigDecimal` operations fail
pub fn linear(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	if points.is_empty() {
		bail!(Error::InsufficientPointsError);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	if points.len() < Spline::Linear.number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	// Fast path for very small datasets
	if points.len() == 2 {
		return linear_two_point_fast(&points, start, end, resolution);
	}

	// Sort points by timestamp
	let mut sorted_points = points;
	sorted_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Check for uniform spacing
	if is_uniformly_spaced(&sorted_points, resolution) {
		return linear_uniform_fast(&sorted_points, start, end, resolution);
	}

	let step = resolution.to_step();

	// Build optimized linear spline
	let spline = LinearSpline::new(&sorted_points, resolution)?;

	let mut result = Vec::new();

	// Pre-compute rounding to avoid repeated calculations - fix negative timestamp handling
	let start_base = match resolution {
		Resolution::Nanoseconds => start.timestamp_subsec_nanos() as i64,
		Resolution::Microseconds => start.timestamp_subsec_micros() as i64,
		Resolution::Milliseconds => start.timestamp_subsec_millis() as i64,
		Resolution::Seconds => start.timestamp(),
		Resolution::Minutes => start.timestamp() * SECONDS_IN_MINUTE,
		Resolution::Hours => start.timestamp() * SECONDS_IN_HOUR,
		Resolution::Days => start.timestamp() * SECONDS_IN_DAY,
		Resolution::Weeks => start.timestamp() * SECONDS_IN_WEEK,
		Resolution::Months => start.timestamp() * SECONDS_IN_MONTH,
		Resolution::Years => start.timestamp() * SECONDS_IN_YEAR,
	};

	let step_base = match resolution {
		Resolution::Nanoseconds => match step.num_nanoseconds() {
			Some(nanos) => nanos,
			None => bail!(Error::TimeError("Invalid nanosecond step".to_string())),
		},
		Resolution::Microseconds => match step.num_microseconds() {
			Some(micros) => micros,
			None => bail!(Error::TimeError("Invalid microsecond step".to_string())),
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
		Resolution::Nanoseconds => end.timestamp_subsec_nanos() as i64,
		Resolution::Microseconds => end.timestamp_subsec_micros() as i64,
		Resolution::Milliseconds => end.timestamp_subsec_millis() as i64,
		Resolution::Seconds => end.timestamp(),
		Resolution::Minutes => end.timestamp() * SECONDS_IN_MINUTE,
		Resolution::Hours => end.timestamp() * SECONDS_IN_HOUR,
		Resolution::Days => end.timestamp() * SECONDS_IN_DAY,
		Resolution::Weeks => end.timestamp() * SECONDS_IN_WEEK,
		Resolution::Months => end.timestamp() * SECONDS_IN_MONTH,
		Resolution::Years => end.timestamp() * SECONDS_IN_YEAR,
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
		Resolution::Nanoseconds => rounded_end.timestamp_subsec_nanos() as i64 - rounded_start.timestamp_subsec_nanos() as i64,
		Resolution::Microseconds => rounded_end.timestamp_subsec_micros() as i64 - rounded_start.timestamp_subsec_micros() as i64,
		Resolution::Milliseconds => rounded_end.timestamp_subsec_millis() as i64 - rounded_start.timestamp_subsec_millis() as i64,
		Resolution::Seconds => rounded_end.timestamp() - rounded_start.timestamp(),
		Resolution::Minutes => rounded_end.timestamp() * SECONDS_IN_MINUTE - rounded_start.timestamp() * SECONDS_IN_MINUTE,
		Resolution::Hours => rounded_end.timestamp() * SECONDS_IN_HOUR - rounded_start.timestamp() * SECONDS_IN_HOUR,
		Resolution::Days => rounded_end.timestamp() * SECONDS_IN_DAY - rounded_start.timestamp() * SECONDS_IN_DAY,
		Resolution::Weeks => rounded_end.timestamp() * SECONDS_IN_WEEK - rounded_start.timestamp() * SECONDS_IN_WEEK,
		Resolution::Months => (rounded_end.timestamp() * SECONDS_IN_MONTH) - (rounded_start.timestamp() * SECONDS_IN_MONTH),
		Resolution::Years => (rounded_end.timestamp() * SECONDS_IN_YEAR) - (rounded_start.timestamp() * SECONDS_IN_YEAR),
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
		let value = spline.evaluate(current_time)?;
		result.push(Point { timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Fast path for uniformly spaced data
fn linear_uniform_fast(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval - check for zero interval
	let uniform_interval = match resolution {
		Resolution::Nanoseconds => match (points[1].timestamp - points[0].timestamp).num_nanoseconds() {
			Some(interval) => interval,
			_ => bail!(Error::TimeError("Invalid nanosecond interval".to_string())),
		},
		Resolution::Microseconds => match (points[1].timestamp - points[0].timestamp).num_microseconds() {
			Some(interval) => interval,
			_ => bail!(Error::TimeError("Invalid microsecond interval".to_string())),
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

	// Handle case where points have identical or nearly identical timestamps
	if uniform_interval == 0 {
		// All points have the same timestamp - use constant value interpolation
		let constant_value = &points[0].value;
		let mut current_time = start;

		while current_time <= end {
			result.push(Point { timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	let uniform_interval = match BigDecimal::from_i64(uniform_interval) {
		Some(interval) => interval,
		None => bail!(Error::DecimalConversionError),
	};

	let mut current_time = start;
	let data_start = points[0].timestamp;
	let data_end = points[points.len() - 1].timestamp;

	while current_time <= end {
		let value = if current_time < data_start || current_time > data_end {
			// Extrapolation - use boundary segments
			if current_time < data_start {
				// Extrapolate backward using first segment
				let dt = match resolution {
					Resolution::Nanoseconds => {
						let delta = match (current_time - data_start).num_nanoseconds() {
							Some(nanos) => nanos,
							None => bail!(Error::TimeError("Invalid nanosecond difference".to_string())),
						};
						let res = match BigDecimal::from_i64(delta) {
							Some(value) => value,
							None => bail!(Error::DecimalConversionError),
						};
						res
					}
					Resolution::Microseconds => {
						let delta = match (current_time - data_start).num_microseconds() {
							Some(micros) => micros,
							None => bail!(Error::TimeError("Invalid microsecond difference".to_string())),
						};
						let res = match BigDecimal::from_i64(delta) {
							Some(value) => value,
							None => bail!(Error::DecimalConversionError),
						};
						res
					}
					Resolution::Milliseconds => match BigDecimal::from_i64((current_time - data_start).num_milliseconds()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Seconds => match BigDecimal::from_i64((current_time - data_start).num_seconds()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Minutes => match BigDecimal::from_i64((current_time - data_start).num_minutes()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Hours => match BigDecimal::from_i64((current_time - data_start).num_hours()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Days => match BigDecimal::from_i64((current_time - data_start).num_days()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Weeks => match BigDecimal::from_i64((current_time - data_start).num_weeks()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Months => {
						match BigDecimal::from_i64((current_time - data_start).num_days() / DAYS_IN_MONTH) {
							// Approximate month as 30 days
							Some(value) => value,
							None => bail!(Error::DecimalConversionError),
						}
					}
					Resolution::Years => {
						match BigDecimal::from_i64((current_time - data_start).num_days() / DAYS_IN_YEAR) {
							// Approximate year as 365 days
							Some(value) => value,
							None => bail!(Error::DecimalConversionError),
						}
					}
				};
				let slope = (&points[1].value - &points[0].value) / &uniform_interval;
				&points[0].value + slope * dt
			} else {
				// Extrapolate forward using last segment
				let n = points.len();
				let dt = match resolution {
					Resolution::Nanoseconds => {
						let delta = match (current_time - points[n - 1].timestamp).num_nanoseconds() {
							Some(nanos) => nanos,
							None => bail!(Error::TimeError("Invalid nanosecond difference".to_string())),
						};
						match BigDecimal::from_i64(delta) {
							Some(value) => value,
							None => bail!(Error::DecimalConversionError),
						}
					}
					Resolution::Microseconds => {
						let delta = match (current_time - points[n - 1].timestamp).num_microseconds() {
							Some(micros) => micros,
							None => bail!(Error::TimeError("Invalid microsecond difference".to_string())),
						};
						match BigDecimal::from_i64(delta) {
							Some(value) => value,
							None => bail!(Error::DecimalConversionError),
						}
					}
					Resolution::Milliseconds => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_milliseconds()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Seconds => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_seconds()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Minutes => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_minutes()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Hours => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_hours()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Days => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_days()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Weeks => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_weeks()) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Months => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_days() / DAYS_IN_MONTH) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
					Resolution::Years => match BigDecimal::from_i64((current_time - points[n - 1].timestamp).num_days() / DAYS_IN_YEAR) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					},
				};
				let slope = (&points[n - 1].value - &points[n - 2].value) / &uniform_interval;
				&points[n - 2].value + slope * dt
			}
		} else {
			// Interpolation - use uniform spacing optimization with safe casting
			let time_from_start = match resolution {
				Resolution::Nanoseconds => match (current_time - data_start).num_nanoseconds() {
					Some(nanos) => nanos,
					None => bail!(Error::TimeError("Invalid nanosecond difference".to_string())),
				},
				Resolution::Microseconds => match (current_time - data_start).num_microseconds() {
					Some(micros) => micros,
					None => bail!(Error::TimeError("Invalid microsecond difference".to_string())),
				},
				Resolution::Milliseconds => (current_time - data_start).num_milliseconds(),
				Resolution::Seconds => (current_time - data_start).num_seconds(),
				Resolution::Minutes => (current_time - data_start).num_minutes(),
				Resolution::Hours => (current_time - data_start).num_hours(),
				Resolution::Days => (current_time - data_start).num_days(),
				Resolution::Weeks => (current_time - data_start).num_weeks(),
				Resolution::Months => (current_time - data_start).num_days() / DAYS_IN_MONTH,
				Resolution::Years => (current_time - data_start).num_days() / DAYS_IN_YEAR,
			};
			let segment_index = if time_from_start >= 0 && uniform_interval > BigDecimal::zero() { (time_from_start / uniform_interval.clone()).to_usize().unwrap_or(0).min(points.len().saturating_sub(2)) } else { 0 };
			let segment_start_time = points[segment_index].timestamp;
			let dt = match resolution {
				Resolution::Nanoseconds => {
					let delta = match (current_time - segment_start_time).num_nanoseconds() {
						Some(nanos) => nanos,
						None => bail!(Error::TimeError("Invalid nanosecond difference".to_string())),
					};
					match BigDecimal::from_i64(delta) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					}
				}
				Resolution::Microseconds => {
					let delta = match (current_time - segment_start_time).num_microseconds() {
						Some(micros) => micros,
						None => bail!(Error::TimeError("Invalid microsecond difference".to_string())),
					};
					match BigDecimal::from_i64(delta) {
						Some(value) => value,
						None => bail!(Error::DecimalConversionError),
					}
				}
				Resolution::Milliseconds => match BigDecimal::from_i64((current_time - segment_start_time).num_milliseconds()) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Seconds => match BigDecimal::from_i64((current_time - segment_start_time).num_seconds()) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Minutes => match BigDecimal::from_i64((current_time - segment_start_time).num_minutes()) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Hours => match BigDecimal::from_i64((current_time - segment_start_time).num_hours()) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Days => match BigDecimal::from_i64((current_time - segment_start_time).num_days()) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Weeks => match BigDecimal::from_i64((current_time - segment_start_time).num_weeks()) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Months => match BigDecimal::from_i64((current_time - segment_start_time).num_days() / DAYS_IN_MONTH) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
				Resolution::Years => match BigDecimal::from_i64((current_time - segment_start_time).num_days() / DAYS_IN_YEAR) {
					Some(value) => value,
					None => bail!(Error::DecimalConversionError),
				},
			};
			let slope = (&points[segment_index + 1].value - &points[segment_index].value) / &uniform_interval;
			&points[segment_index].value + slope * dt
		};

		result.push(Point { timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Optimized two-point linear interpolation
fn linear_two_point_fast(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Check for identical timestamps first
	let dt_base = match resolution {
		Resolution::Nanoseconds => match (points[1].timestamp - points[0].timestamp).num_nanoseconds() {
			Some(nanos) => nanos,
			None => bail!(Error::TimeError("Invalid nanosecond difference".to_string())),
		},
		Resolution::Microseconds => match (points[1].timestamp - points[0].timestamp).num_microseconds() {
			Some(micros) => micros,
			None => bail!(Error::TimeError("Invalid microsecond difference".to_string())),
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

	if dt_base == 0 {
		// Handle identical timestamps - use constant value interpolation
		let constant_value = &points[0].value;
		let mut current_time = start;

		while current_time <= end {
			result.push(Point { timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	// Pre-compute slope once (safe now that we know dt_base != 0)
	let dt = match BigDecimal::from_i64(dt_base) {
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
				Some(nanos) => nanos,
				None => bail!(Error::TimeError("Invalid nanosecond difference".to_string())),
			},
			Resolution::Microseconds => match (current_time - base_time).num_microseconds() {
				Some(micros) => micros,
				None => bail!(Error::TimeError("Invalid microsecond difference".to_string())),
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

/// SIMD-optimized linear interpolation for batch processing
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points (< 2 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn linear_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution) -> Result<Vec<Point>> {
	if points.len() < 2 {
		bail!(Error::InsufficientPointsError);
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Helper to convert DateTime<Utc> to f64 based on resolution
	let time_to_f64 = |t: &DateTime<Utc>| -> f64 {
		#[allow(clippy::cast_precision_loss)]
		match resolution {
			Resolution::Nanoseconds => t.timestamp_nanos_opt().unwrap_or(0) as f64,
			Resolution::Microseconds => t.timestamp_micros() as f64,
			Resolution::Milliseconds => t.timestamp_millis() as f64,
			Resolution::Seconds => t.timestamp() as f64,
			Resolution::Minutes => (t.timestamp() / SECONDS_IN_MINUTE) as f64,
			Resolution::Hours => (t.timestamp() / SECONDS_IN_HOUR) as f64,
			Resolution::Days => (t.timestamp() / SECONDS_IN_DAY) as f64,
			Resolution::Weeks => (t.timestamp() / SECONDS_IN_WEEK) as f64,
			Resolution::Months => (t.timestamp() / SECONDS_IN_MONTH) as f64,
			Resolution::Years => (t.timestamp() / SECONDS_IN_YEAR) as f64,
		}
	};

	// Convert to f64 arrays for SIMD processing
	#[allow(clippy::cast_precision_loss)]
	let input_times: Vec<f64> = points.iter().map(|m| time_to_f64(&m.timestamp)).collect();
	let input_values: Vec<f64> = points.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

	#[allow(clippy::cast_precision_loss)]
	let targets: Vec<f64> = target_times.iter().map(time_to_f64).collect();

	// Process in SIMD batches
	let mut results = Vec::with_capacity(target_times.len());

	for chunk in targets.chunks(SIMD_BATCH_SIZE) {
		let mut padded_targets = [0.0; SIMD_BATCH_SIZE];
		let chunk_size = chunk.len();

		// Copy actual values and pad with last value if needed
		padded_targets[..chunk_size].copy_from_slice(chunk);

		// Fill remaining slots with the last value
		if chunk_size < SIMD_BATCH_SIZE {
			let last_value = chunk.last().copied().unwrap_or(0.0);
			padded_targets[chunk_size..].fill(last_value);
		}

		let target_simd = f64x4::from(padded_targets);
		let result_simd = simd_linear_interpolate(&input_times, &input_values, target_simd);

		// Extract results (only take the actual chunk size)
		let result_array = result_simd.to_array();
		results.extend_from_slice(&result_array[..chunk_size]);
	}

	// Convert back to points
	let interpolated = results.into_iter().enumerate().map(|(i, value)| Point { timestamp: target_times[i], value: BigDecimal::from_f64(value).unwrap_or_else(BigDecimal::zero) }).collect();

	Ok(interpolated)
}

/// SIMD linear interpolation core function
fn simd_linear_interpolate(input_times: &[f64], input_values: &[f64], target_times: f64x4) -> f64x4 {
	// Find the segment index for each of the 4 target times.
	// `partition_point` is faster than a linear scan, returning the index
	// of the first element `x` for which `f(x)` is false.
	// We subtract 1 to get the index of the start of the segment.
	let indices: [usize; 4] = target_times.to_array().map(|t| {
		let idx = input_times.partition_point(|&it| it < t);
		// Handle extrapolation cases properly
		if idx == 0 {
			0 // Before first point - use first segment
		} else if idx >= input_times.len() {
			input_times.len().saturating_sub(2) // After last point - use last segment
		} else {
			idx.saturating_sub(1) // Normal case - use segment before the partition point
		}
	});

	// Gather values from the input slices based on the found indices.
	// This loads the start and end points of the segments for all 4 lanes.
	let t0 = f64x4::new([input_times[indices[0]], input_times[indices[1]], input_times[indices[2]], input_times[indices[3]]]);
	let t1 = f64x4::new([input_times[indices[0] + 1], input_times[indices[1] + 1], input_times[indices[2] + 1], input_times[indices[3] + 1]]);
	let v0 = f64x4::new([input_values[indices[0]], input_values[indices[1]], input_values[indices[2]], input_values[indices[3]]]);
	let v1 = f64x4::new([input_values[indices[0] + 1], input_values[indices[1] + 1], input_values[indices[2] + 1], input_values[indices[3] + 1]]);

	// Perform linear interpolation using SIMD operations.
	// alpha = (target - t0) / (t1 - t0)
	let alpha = (target_times - t0) / (t1 - t0);

	// result = v0 + alpha * (v1 - v0)
	// Using mul_add for a potential fused multiply-add (FMA) optimization.
	alpha.mul_add(v1 - v0, v0)
}
