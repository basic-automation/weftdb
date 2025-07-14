use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};

use crate::{Error, Point, Resolution, Spline};

pub const SECONDS_IN_MINUTE: i64 = 60;
pub const SECONDS_IN_HOUR: i64 = 3_600;
pub const SECONDS_IN_DAY: i64 = 86_400;
pub const SECONDS_IN_WEEK: i64 = 604_800;
pub const SECONDS_IN_MONTH: i64 = 2_592_000;
pub const SECONDS_IN_YEAR: i64 = 31_536_000;
pub const DAYS_IN_MONTH: i64 = 30;
pub const DAYS_IN_YEAR: i64 = 365;

pub const SIMD_BATCH_SIZE: usize = 4;

/// Linear spline implementation for efficient interpolation
pub struct LinearSpline {
	segments: Vec<LinearSegment>,
	time_bounds: Vec<DateTime<Utc>>,
	resolution: Resolution,
}

impl LinearSpline {
	pub fn new(points: &[Point], resolution: Resolution) -> Result<Self> {
		let mut segments = Vec::with_capacity(points.len().saturating_sub(1));
		let mut time_bounds = Vec::with_capacity(points.len());

		for i in 0..points.len() - 1 {
			let segment = LinearSegment::fit_linear(&points[i], &points[i + 1], resolution)?;
			segments.push(segment);
			time_bounds.push(points[i].timestamp);
		}
		time_bounds.push(points[points.len() - 1].timestamp);

		Ok(Self { segments, time_bounds, resolution })
	}

	pub fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		// Handle boundary cases
		if target_time <= self.time_bounds[0] {
			return Ok(self.segments[0].intercept.clone());
		}
		if target_time >= self.time_bounds[self.time_bounds.len() - 1] {
			return Ok(self.segments[self.segments.len() - 1].evaluate_at_end());
		}

		// Find the appropriate segment
		for (i, &bound_time) in self.time_bounds.iter().enumerate().skip(1) {
			if target_time <= bound_time {
				let segment_idx = i - 1;
				let dt_base = match self.resolution {
					Resolution::Nanoseconds => match (target_time - self.time_bounds[segment_idx]).num_nanoseconds() {
						Some(value) => value,
						None => bail!(Error::InvalidTimeRangeError),
					},
					Resolution::Microseconds => match (target_time - self.time_bounds[segment_idx]).num_microseconds() {
						Some(value) => value,
						None => bail!(Error::InvalidTimeRangeError),
					},
					Resolution::Milliseconds => (target_time - self.time_bounds[segment_idx]).num_milliseconds(),
					Resolution::Seconds => (target_time - self.time_bounds[segment_idx]).num_seconds(),
					Resolution::Minutes => (target_time - self.time_bounds[segment_idx]).num_minutes(),
					Resolution::Hours => (target_time - self.time_bounds[segment_idx]).num_hours(),
					Resolution::Days => (target_time - self.time_bounds[segment_idx]).num_days(),
					Resolution::Weeks => (target_time - self.time_bounds[segment_idx]).num_weeks(),
					Resolution::Months => (target_time - self.time_bounds[segment_idx]).num_days() / DAYS_IN_MONTH,
					Resolution::Years => (target_time - self.time_bounds[segment_idx]).num_days() / DAYS_IN_YEAR,
				};
				let dt = match BigDecimal::from_i64(dt_base) {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				};
				return Ok(self.segments[segment_idx].evaluate(dt));
			}
		}

		// Fallback to last segment
		let last_idx = self.segments.len() - 1;
		let dt_base = match self.resolution {
			Resolution::Nanoseconds => match (target_time - self.time_bounds[last_idx]).num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Microseconds => match (target_time - self.time_bounds[last_idx]).num_microseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Milliseconds => (target_time - self.time_bounds[last_idx]).num_milliseconds(),
			Resolution::Seconds => (target_time - self.time_bounds[last_idx]).num_seconds(),
			Resolution::Minutes => (target_time - self.time_bounds[last_idx]).num_minutes(),
			Resolution::Hours => (target_time - self.time_bounds[last_idx]).num_hours(),
			Resolution::Days => (target_time - self.time_bounds[last_idx]).num_days(),
			Resolution::Weeks => (target_time - self.time_bounds[last_idx]).num_weeks(),
			Resolution::Months => (target_time - self.time_bounds[last_idx]).num_days() / DAYS_IN_MONTH,
			Resolution::Years => (target_time - self.time_bounds[last_idx]).num_days() / DAYS_IN_YEAR,
		};
		let dt = match BigDecimal::from_i64(dt_base) {
			Some(value) => value,
			None => bail!(Error::InvalidTimeRangeError),
		};
		Ok(self.segments[last_idx].evaluate(dt))
	}
}

/// Linear segment with slope and intercept
#[derive(Debug, Clone)]
struct LinearSegment {
	slope: BigDecimal,
	intercept: BigDecimal,
	end_value: BigDecimal, // Store end value for identical timestamps
}

impl LinearSegment {
	/// Create a linear segment between two measurements
	fn fit_linear(p1: &Point, p2: &Point, resolution: Resolution) -> Result<Self> {
		let dt_base = match resolution {
			Resolution::Nanoseconds => match (p2.timestamp - p1.timestamp).num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Microseconds => match (p2.timestamp - p1.timestamp).num_microseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Milliseconds => (p2.timestamp - p1.timestamp).num_milliseconds(),
			Resolution::Seconds => (p2.timestamp - p1.timestamp).num_seconds(),
			Resolution::Minutes => (p2.timestamp - p1.timestamp).num_minutes(),
			Resolution::Hours => (p2.timestamp - p1.timestamp).num_hours(),
			Resolution::Days => (p2.timestamp - p1.timestamp).num_days(),
			Resolution::Weeks => (p2.timestamp - p1.timestamp).num_weeks(),
			Resolution::Months => (p2.timestamp - p1.timestamp).num_days() / DAYS_IN_MONTH,
			Resolution::Years => (p2.timestamp - p1.timestamp).num_days() / DAYS_IN_YEAR,
		};

		if dt_base == 0 {
			// Handle identical timestamps - create constant segment
			return Ok(Self {
				slope: BigDecimal::zero(),
				intercept: p1.value.clone(),
				end_value: p1.value.clone(), // Use first value for consistency
			});
		}

		let dt = match BigDecimal::from_i64(dt_base) {
			Some(value) => value,
			None => bail!(Error::InvalidTimeRangeError),
		};
		let dy = &p2.value - &p1.value;
		let slope = dy / dt;

		// y = mx + b, where b is the y-intercept at the start time
		let intercept = p1.value.clone();
		let end_value = p2.value.clone();

		Ok(Self { slope, intercept, end_value })
	}

	/// Evaluate the linear function at time offset dt (in milliseconds)
	fn evaluate(&self, dt: BigDecimal) -> BigDecimal {
		&self.intercept + &self.slope * dt
	}

	/// Get the end value of this segment
	fn evaluate_at_end(&self) -> BigDecimal {
		self.end_value.clone()
	}
}

pub struct QuadraticSpline {
	points: Vec<Point>,
	coefficients: Vec<QuadraticSegment>,
}

struct QuadraticSegment {
	a: BigDecimal, // quadratic coefficient
	b: BigDecimal, // linear coefficient
	c: BigDecimal, // constant coefficient
}

impl QuadraticSpline {
	pub fn new(points: &[Point], resolution: Resolution) -> Result<Self> {
		let n = points.len();
		if n < 2 {
			bail!(Error::InsufficientPointsError);
		}

		let mut coefficients = Vec::with_capacity(n - 1);

		// Pre-compute all segments for better cache locality
		for i in 0..n - 1 {
			let segment = Self::fit_quadratic_segment(points, i, resolution)?;
			coefficients.push(segment);
		}

		Ok(Self { points: points.to_vec(), coefficients })
	}

	/// Fit a quadratic segment using local points
	fn fit_quadratic_segment(points: &[Point], segment_idx: usize, resolution: Resolution) -> Result<QuadraticSegment> {
		let n = points.len();

		// Choose three points for quadratic fitting
		let (p0, p1, p2) = if segment_idx == 0 && n >= 3 {
			// Use first three points
			(&points[0], &points[1], &points[2])
		} else if segment_idx >= n - 2 && n >= 3 {
			// Use last three points
			(&points[n - 3], &points[n - 2], &points[n - 1])
		} else if n >= 3 {
			// Use centered three points
			(&points[segment_idx], &points[segment_idx + 1], &points[segment_idx.min(n - 2)])
		} else {
			// Fall back to linear for insufficient points
			let p1 = &points[segment_idx];
			let p2 = &points[segment_idx + 1];

			let dt = match resolution {
				Resolution::Nanoseconds => match (p2.timestamp - p1.timestamp).num_nanoseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Microseconds => match (p2.timestamp - p1.timestamp).num_microseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Milliseconds => (p2.timestamp - p1.timestamp).num_milliseconds(),
				Resolution::Seconds => (p2.timestamp - p1.timestamp).num_seconds(),
				Resolution::Minutes => (p2.timestamp - p1.timestamp).num_minutes(),
				Resolution::Hours => (p2.timestamp - p1.timestamp).num_hours(),
				Resolution::Days => (p2.timestamp - p1.timestamp).num_days(),
				Resolution::Weeks => (p2.timestamp - p1.timestamp).num_weeks(),
				Resolution::Months => (p2.timestamp - p1.timestamp).num_days() / DAYS_IN_MONTH,
				Resolution::Years => (p2.timestamp - p1.timestamp).num_days() / DAYS_IN_YEAR,
			};
			let dt = match BigDecimal::from_i64(dt) {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			};

			if dt.is_zero() {
				return Ok(QuadraticSegment { a: BigDecimal::zero(), b: BigDecimal::zero(), c: p1.value.clone() });
			}

			let dy = &p2.value - &p1.value;
			let slope = dy / dt;

			return Ok(QuadraticSegment { a: BigDecimal::zero(), b: slope, c: p1.value.clone() });
		};

		// Convert timestamps to relative time from p1 for numerical stability
		let base_time = p1.timestamp;
		let t0 = match resolution {
			Resolution::Nanoseconds => match (p0.timestamp - base_time).num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Microseconds => match (p0.timestamp - base_time).num_microseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Milliseconds => (p0.timestamp - base_time).num_milliseconds(),
			Resolution::Seconds => (p0.timestamp - base_time).num_seconds(),
			Resolution::Minutes => (p0.timestamp - base_time).num_minutes(),
			Resolution::Hours => (p0.timestamp - base_time).num_hours(),
			Resolution::Days => (p0.timestamp - base_time).num_days(),
			Resolution::Weeks => (p0.timestamp - base_time).num_weeks(),
			Resolution::Months => (p0.timestamp - base_time).num_days() / DAYS_IN_MONTH,
			Resolution::Years => (p0.timestamp - base_time).num_days() / DAYS_IN_YEAR,
		};
		let t0 = match BigDecimal::from_i64(t0) {
			Some(value) => value,
			None => bail!(Error::InvalidTimeRangeError),
		};
		let t1 = BigDecimal::zero(); // p1 is at time 0
		let t2 = match resolution {
			Resolution::Nanoseconds => match (p2.timestamp - base_time).num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Microseconds => match (p2.timestamp - base_time).num_microseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Milliseconds => (p2.timestamp - base_time).num_milliseconds(),
			Resolution::Seconds => (p2.timestamp - base_time).num_seconds(),
			Resolution::Minutes => (p2.timestamp - base_time).num_minutes(),
			Resolution::Hours => (p2.timestamp - base_time).num_hours(),
			Resolution::Days => (p2.timestamp - base_time).num_days(),
			Resolution::Weeks => (p2.timestamp - base_time).num_weeks(),
			Resolution::Months => (p2.timestamp - base_time).num_days() / DAYS_IN_MONTH,
			Resolution::Years => (p2.timestamp - base_time).num_days() / DAYS_IN_YEAR,
		};
		let t2 = match BigDecimal::from_i64(t2) {
			Some(value) => value,
			None => bail!(Error::InvalidTimeRangeError),
		};

		// Solve quadratic system: y = at² + bt + c
		// Using Lagrange interpolation for numerical stability

		// Calculate denominators for Lagrange basis functions
		let denom_0 = (&t0 - &t1) * (&t0 - &t2);
		let denom_1 = (&t1 - &t0) * (&t1 - &t2);
		let denom_2 = (&t2 - &t0) * (&t2 - &t1);

		if denom_0.is_zero() || denom_1.is_zero() || denom_2.is_zero() {
			// Fall back to linear interpolation if points are collinear in time
			let dt = &t2 - &t0;
			if dt.is_zero() {
				return Ok(QuadraticSegment { a: BigDecimal::zero(), b: BigDecimal::zero(), c: p1.value.clone() });
			}

			let dy = &p2.value - &p0.value;
			let slope = dy / dt;

			return Ok(QuadraticSegment { a: BigDecimal::zero(), b: slope, c: p1.value.clone() });
		}

		// Calculate quadratic coefficients using Lagrange method - remove unused variable
		// For efficiency, we compute the coefficients directly

		// Coefficient of t² term
		let a = (&p0.value / &denom_0) + (&p1.value / &denom_1) + (&p2.value / &denom_2);

		// Coefficient of t term
		let b = (&p0.value * (&t1 + &t2) / (-&denom_0)) + (&p1.value * (&t0 + &t2) / (-&denom_1)) + (&p2.value * (&t0 + &t1) / (-&denom_2));

		// Constant term (value at t=0, which is p1)
		let c = p1.value.clone();

		Ok(QuadraticSegment { a, b, c })
	}

	/// Optimized segment evaluation with reference passing
	fn evaluate_segment(&self, segment_idx: usize, dt: &BigDecimal) -> BigDecimal {
		let seg = &self.coefficients[segment_idx];
		let dt_squared = dt * dt;

		// Evaluate: a*t² + b*t + c
		&seg.a * dt_squared + &seg.b * dt + &seg.c
	}

	pub fn evaluate(&self, target_time: DateTime<Utc>, resolution: Resolution) -> Result<BigDecimal> {
		let n = self.points.len();

		// Handle extrapolation backward
		if target_time <= self.points[0].timestamp {
			let dt = match resolution {
				Resolution::Nanoseconds => match (target_time - self.points[0].timestamp).num_nanoseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Microseconds => match (target_time - self.points[0].timestamp).num_microseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Milliseconds => (target_time - self.points[0].timestamp).num_milliseconds(),
				Resolution::Seconds => (target_time - self.points[0].timestamp).num_seconds(),
				Resolution::Minutes => (target_time - self.points[0].timestamp).num_minutes(),
				Resolution::Hours => (target_time - self.points[0].timestamp).num_hours(),
				Resolution::Days => (target_time - self.points[0].timestamp).num_days(),
				Resolution::Weeks => (target_time - self.points[0].timestamp).num_weeks(),
				Resolution::Months => (target_time - self.points[0].timestamp).num_days() / DAYS_IN_MONTH,
				Resolution::Years => (target_time - self.points[0].timestamp).num_days() / DAYS_IN_YEAR,
			};

			let dt = match BigDecimal::from_i64(dt) {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			};
			return Ok(self.evaluate_segment(0, &dt));
		}

		// Handle extrapolation forward
		if target_time >= self.points[n - 1].timestamp {
			let dt = match resolution {
				Resolution::Nanoseconds => match (target_time - self.points[n - 1].timestamp).num_nanoseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Microseconds => match (target_time - self.points[n - 1].timestamp).num_microseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Milliseconds => (target_time - self.points[n - 1].timestamp).num_milliseconds(),
				Resolution::Seconds => (target_time - self.points[n - 1].timestamp).num_seconds(),
				Resolution::Minutes => (target_time - self.points[n - 1].timestamp).num_minutes(),
				Resolution::Hours => (target_time - self.points[n - 1].timestamp).num_hours(),
				Resolution::Days => (target_time - self.points[n - 1].timestamp).num_days(),
				Resolution::Weeks => (target_time - self.points[n - 1].timestamp).num_weeks(),
				Resolution::Months => (target_time - self.points[n - 1].timestamp).num_days() / DAYS_IN_MONTH,
				Resolution::Years => (target_time - self.points[n - 1].timestamp).num_days() / DAYS_IN_YEAR,
			};

			let dt = match BigDecimal::from_i64(dt) {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			};
			return Ok(self.evaluate_segment(n - 2, &dt));
		}

		// Binary search for the appropriate segment - MAJOR PERFORMANCE IMPROVEMENT
		let segment_idx = self.find_segment_binary(target_time);
		let dt = match resolution {
			Resolution::Nanoseconds => match (target_time - self.points[segment_idx].timestamp).num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Microseconds => match (target_time - self.points[segment_idx].timestamp).num_microseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Milliseconds => (target_time - self.points[segment_idx].timestamp).num_milliseconds(),
			Resolution::Seconds => (target_time - self.points[segment_idx].timestamp).num_seconds(),
			Resolution::Minutes => (target_time - self.points[segment_idx].timestamp).num_minutes(),
			Resolution::Hours => (target_time - self.points[segment_idx].timestamp).num_hours(),
			Resolution::Days => (target_time - self.points[segment_idx].timestamp).num_days(),
			Resolution::Weeks => (target_time - self.points[segment_idx].timestamp).num_weeks(),
			Resolution::Months => (target_time - self.points[segment_idx].timestamp).num_days() / DAYS_IN_MONTH,
			Resolution::Years => (target_time - self.points[segment_idx].timestamp).num_days() / DAYS_IN_YEAR,
		};
		let dt = match BigDecimal::from_i64(dt) {
			Some(value) => value,
			None => bail!(Error::InvalidTimeRangeError),
		};

		Ok(self.evaluate_segment(segment_idx, &dt))
	}

	/// Binary search for segment - O(log n) instead of O(n)
	fn find_segment_binary(&self, target_time: DateTime<Utc>) -> usize {
		let mut left = 0;
		let mut right = self.points.len() - 1;

		while left < right - 1 {
			let mid = left + (right - left) / 2;
			if target_time < self.points[mid].timestamp {
				right = mid;
			} else {
				left = mid;
			}
		}

		left
	}
}

#[derive(Debug, Clone)]
struct CubicSegment {
	a: BigDecimal, // y value
	b: BigDecimal, // first derivative
	c: BigDecimal, // second derivative / 2
	d: BigDecimal, // third derivative / 6
}

#[derive(Debug, Clone)]
pub struct CubicSpline {
	points: Vec<Point>,
	coefficients: Vec<CubicSegment>,
	resolution: Resolution,
}

impl CubicSpline {
	/// Creates a new cubic spline from measurements
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Insufficient measurements for cubic spline interpolation
	/// - Timestamp conversion or `BigDecimal` operations fail
	pub fn new(points: &[Point], resolution: Resolution) -> Result<Self> {
		let n = points.len();
		if n < Spline::Cubic.number_of_points_required() {
			return Err(Error::InsufficientPointsError.into());
		}

		Self::create_natural_cubic_spline(points, resolution)
	}

	fn create_natural_cubic_spline(points: &[Point], resolution: Resolution) -> Result<Self> {
		let n = points.len();
		let mut coefficients = Vec::with_capacity(n - 1);

		// Simplified cubic interpolation using linear segments
		for i in 0..n - 1 {
			let x0 = match resolution {
				Resolution::Nanoseconds => match points[i].timestamp.timestamp_nanos_opt() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Microseconds => points[i].timestamp.timestamp_micros(),
				Resolution::Milliseconds => points[i].timestamp.timestamp_millis(),
				Resolution::Seconds => points[i].timestamp.timestamp(),
				Resolution::Minutes => points[i].timestamp.timestamp() / SECONDS_IN_MINUTE,
				Resolution::Hours => points[i].timestamp.timestamp() / SECONDS_IN_HOUR,
				Resolution::Days => points[i].timestamp.timestamp() / SECONDS_IN_DAY,
				Resolution::Weeks => points[i].timestamp.timestamp() / SECONDS_IN_WEEK,
				Resolution::Months => points[i].timestamp.timestamp() / (DAYS_IN_MONTH * SECONDS_IN_DAY),
				Resolution::Years => points[i].timestamp.timestamp() / (DAYS_IN_YEAR * SECONDS_IN_DAY),
			};

			let x0 = match BigDecimal::from_i64(x0) {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			};
			let y0 = points[i].value.clone();

			let x1 = match resolution {
				Resolution::Nanoseconds => match points[i + 1].timestamp.timestamp_nanos_opt() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Microseconds => points[i + 1].timestamp.timestamp_micros(),
				Resolution::Milliseconds => points[i + 1].timestamp.timestamp_millis(),
				Resolution::Seconds => points[i + 1].timestamp.timestamp(),
				Resolution::Minutes => points[i + 1].timestamp.timestamp() / SECONDS_IN_MINUTE,
				Resolution::Hours => points[i + 1].timestamp.timestamp() / SECONDS_IN_HOUR,
				Resolution::Days => points[i + 1].timestamp.timestamp() / SECONDS_IN_DAY,
				Resolution::Weeks => points[i + 1].timestamp.timestamp() / SECONDS_IN_WEEK,
				Resolution::Months => points[i + 1].timestamp.timestamp() / (DAYS_IN_MONTH * SECONDS_IN_DAY),
				Resolution::Years => points[i + 1].timestamp.timestamp() / (DAYS_IN_YEAR * SECONDS_IN_DAY),
			};
			let x1 = match BigDecimal::from_i64(x1) {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			};
			let y1 = points[i + 1].value.clone();

			let dx = &x1 - &x0;
			let dy = &y1 - &y0;
			let slope = if dx.is_zero() { BigDecimal::zero() } else { &dy / &dx };

			coefficients.push(CubicSegment { a: y0, b: slope, c: BigDecimal::zero(), d: BigDecimal::zero() });
		}

		Ok(Self { points: points.to_vec(), coefficients, resolution })
	}

	fn find_segment(&self, target_time: DateTime<Utc>) -> usize {
		for (i, measurement) in self.points.iter().enumerate() {
			if target_time <= measurement.timestamp {
				return i.saturating_sub(1);
			}
		}
		self.points.len().saturating_sub(2)
	}

	fn evaluate_segment(&self, segment_idx: usize, dt: &BigDecimal) -> BigDecimal {
		let segment = &self.coefficients[segment_idx];
		let dt2 = dt * dt;
		let dt3 = dt * &dt2;

		&segment.a + &segment.b * dt + &segment.c * &dt2 + &segment.d * &dt3
	}

	/// Evaluates the cubic spline at a target time
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Timestamp conversion to `BigDecimal` fails
	/// - Spline evaluation encounters numerical errors
	pub fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		let n = self.points.len();
		if n == 0 {
			bail!(Error::InsufficientPointsError);
		}

		// Handle edge cases
		if target_time <= self.points[0].timestamp {
			return Ok(self.evaluate_segment(0, &BigDecimal::zero()));
		}

		if target_time >= self.points[n - 1].timestamp {
			let duration = target_time - self.points[n - 2].timestamp;
			let dt = match self.resolution {
				Resolution::Nanoseconds => match duration.num_nanoseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Microseconds => match duration.num_microseconds() {
					Some(value) => value,
					None => bail!(Error::InvalidTimeRangeError),
				},
				Resolution::Milliseconds => duration.num_milliseconds(),
				Resolution::Seconds => duration.num_seconds(),
				Resolution::Minutes => duration.num_minutes(),
				Resolution::Hours => duration.num_hours(),
				Resolution::Days => duration.num_days(),
				Resolution::Weeks => duration.num_weeks(),
				Resolution::Months => duration.num_days() / DAYS_IN_MONTH,
				Resolution::Years => duration.num_days() / DAYS_IN_YEAR,
			};
			let dt = match BigDecimal::from_i64(dt) {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			};
			return Ok(self.evaluate_segment(n - 2, &dt));
		}

		let segment_idx = self.find_segment(target_time);
		let duration = target_time - self.points[segment_idx].timestamp;
		let dt = match self.resolution {
			Resolution::Nanoseconds => match duration.num_nanoseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Microseconds => match duration.num_microseconds() {
				Some(value) => value,
				None => bail!(Error::InvalidTimeRangeError),
			},
			Resolution::Milliseconds => duration.num_milliseconds(),
			Resolution::Seconds => duration.num_seconds(),
			Resolution::Minutes => duration.num_minutes(),
			Resolution::Hours => duration.num_hours(),
			Resolution::Days => duration.num_days(),
			Resolution::Weeks => duration.num_weeks(),
			Resolution::Months => duration.num_days() / DAYS_IN_MONTH,
			Resolution::Years => duration.num_days() / DAYS_IN_YEAR,
		};
		let dt = match BigDecimal::from_i64(dt) {
			Some(value) => value,
			None => bail!(Error::InvalidTimeRangeError),
		};

		Ok(self.evaluate_segment(segment_idx, &dt))
	}
}
