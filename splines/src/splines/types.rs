use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};

use crate::{Error, Point, Resolution};

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
	pub fn new(points: &[Point], resolution: &Resolution) -> Result<Self> {
		let mut segments = Vec::with_capacity(points.len().saturating_sub(1));
		let mut time_bounds = Vec::with_capacity(points.len());

		for i in 0..points.len() - 1 {
			let segment = LinearSegment::fit_linear(&points[i], &points[i + 1], resolution)?;
			segments.push(segment);
			time_bounds.push(points[i].timestamp);
		}
		time_bounds.push(points[points.len() - 1].timestamp);

		Ok(Self { segments, time_bounds, resolution: *resolution })
	}

	pub fn evaluate(&self, target_time: &DateTime<Utc>) -> Result<BigDecimal> {
		// Handle extrapolation backward - use first segment with proper linear extrapolation
		if *target_time <= self.time_bounds[0] {
			let Some(dt) = BigDecimal::from_i64(self.resolution.difference(target_time, &self.time_bounds[0])?) else {
				bail!(Error::InvalidTimeRangeError);
			};
			return Ok(self.segments[0].evaluate(&dt));
		}

		// Handle extrapolation forward - use last segment with proper linear extrapolation
		if *target_time >= self.time_bounds[self.time_bounds.len() - 1] {
			let last_segment_idx = self.segments.len() - 1;
			let Some(dt) = BigDecimal::from_i64(self.resolution.difference(target_time, &self.time_bounds[last_segment_idx])?) else {
				bail!(Error::InvalidTimeRangeError);
			};
			return Ok(self.segments[last_segment_idx].evaluate(&dt));
		}

		// Find the appropriate segment for interpolation
		for (i, &bound_time) in self.time_bounds.iter().enumerate().skip(1) {
			if *target_time <= bound_time {
				let segment_idx = i - 1;
				let Some(dt) = BigDecimal::from_i64(self.resolution.difference(target_time, &self.time_bounds[segment_idx])?) else {
					bail!(Error::InvalidTimeRangeError);
				};
				return Ok(self.segments[segment_idx].evaluate(&dt));
			}
		}

		// Fallback to last segment
		let last_idx = self.segments.len() - 1;
		let Some(dt) = BigDecimal::from_i64(self.resolution.difference(target_time, &self.time_bounds[last_idx])?) else {
			bail!(Error::InvalidTimeRangeError);
		};
		Ok(self.segments[last_idx].evaluate(&dt))
	}
}

/// Linear segment with slope and intercept
#[derive(Debug, Clone)]
struct LinearSegment {
	slope: BigDecimal,
	intercept: BigDecimal,
}

impl LinearSegment {
	/// Create a linear segment between two measurements
	fn fit_linear(p1: &Point, p2: &Point, resolution: &Resolution) -> Result<Self> {
		let Some(dt) = BigDecimal::from_i64(resolution.difference(&p2.timestamp, &p1.timestamp)?) else {
			bail!(Error::InvalidTimeRangeError);
		};

		if dt == BigDecimal::zero() {
			// Handle identical timestamps - create constant segment
			return Ok(Self { slope: BigDecimal::zero(), intercept: p1.value.clone() });
		}

		let dy = &p2.value - &p1.value;
		let slope = dy / dt;

		// y = mx + b, where b is the y-intercept at the start time
		let intercept = p1.value.clone();

		Ok(Self { slope, intercept })
	}

	/// Evaluate the linear function at time offset dt (in milliseconds)
	fn evaluate(&self, dt: &BigDecimal) -> BigDecimal {
		&self.intercept + &self.slope * dt
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
