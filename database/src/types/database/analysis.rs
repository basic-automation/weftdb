use std::pin::Pin;

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use futures::Stream;
use splimes::{Point, Resolution, Spline};

use crate::{types::AnalysisResult, AspectId, Database, CACHE};

impl Database {
	/// Analyze a single point in time for an aspect using interpolation
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to retrieve measurements
	/// - if interpolation fails
	pub async fn analyze_point(&self, aspect: AspectId, time: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Point> {
		// Check cache first for point analysis
		let cache_key = format!("point_{}_{}_{}_{:?}_{:?}", aspect.as_uuid(), time.timestamp(), time.timestamp_subsec_nanos(), resolution, method);
		if let Some(cached_result) = CACHE.get_point_analysis(&cache_key).await {
			return Ok(Point { timestamp: cached_result.timestamp, value: cached_result.value });
		}

		// Get measurements for this aspect, sorted by timestamp
		let mut measurements = Self::get_aspect_measurements(aspect).await?;

		if measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Sort measurements by timestamp (in case they are not sorted)
		measurements.sort_by_key(|m| m.timestamp);

		// Convert to Points
		let points: Vec<Point> = measurements.iter().map(|m| Point { timestamp: m.timestamp, value: m.value.clone() }).collect();

		// Find the position where time would fit
		let pos = points.binary_search_by_key(&time, |p| p.timestamp);

		match pos {
			Ok(idx) => {
				// Exact match found
				let point = points[idx].clone();
				let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?}"), resolution: format!("{resolution:?}") };
				CACHE.store_point_analysis(&cache_key, &analysis_result).await;
				Ok(point)
			}
			Err(idx) => {
				// Time falls between measurements or outside range
				if idx == 0 {
					// Before first measurement - backward extrapolation
					if points.len() < 2 {
						bail!("Insufficient measurements for extrapolation");
					}
					let value = extrapolate_linear(&points[0], &points[1], time);
					let point = Point { timestamp: time, value };
					let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?} (backward extrapolation)"), resolution: format!("{resolution:?}") };
					CACHE.store_point_analysis(&cache_key, &analysis_result).await;
					Ok(point)
				} else if idx >= points.len() {
					// After last measurement - forward extrapolation
					if points.len() < 2 {
						bail!("Insufficient measurements for extrapolation");
					}
					let value = extrapolate_linear(&points[points.len() - 2], &points[points.len() - 1], time);
					let point = Point { timestamp: time, value };
					let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?} (forward extrapolation)"), resolution: format!("{resolution:?}") };
					CACHE.store_point_analysis(&cache_key, &analysis_result).await;
					Ok(point)
				} else {
					// Interpolation between measurements
					match method {
						Spline::Linear => {
							// Linear interpolation between points[idx-1] and points[idx]
							let value = interpolate_linear(&points[idx - 1], &points[idx], time);
							let point = Point { timestamp: time, value };
							let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?}"), resolution: format!("{resolution:?}") };
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
						Spline::Quadratic => {
							// Need 3 points for quadratic
							if points.len() < 3 {
								bail!("Insufficient measurements for quadratic interpolation");
							}
							// Use three points centered around the target
							let (p1, p2, p3) = if idx == 1 {
								(&points[0], &points[1], &points[2])
							} else if idx == points.len() {
								(&points[idx - 3], &points[idx - 2], &points[idx - 1])
							} else {
								(&points[idx - 2], &points[idx - 1], &points[idx])
							};
							let value = quadratic_interpolate(p1, p2, p3, time);
							let point = Point { timestamp: time, value };
							let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?}"), resolution: format!("{resolution:?}") };
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
						Spline::Cubic => {
							// Need 4 points for cubic
							if points.len() < 4 {
								bail!("Insufficient measurements for cubic interpolation");
							}
							// Use four points centered around the target
							let (p0, p1, p2, p3) = if idx <= 2 {
								(&points[0], &points[1], &points[2], &points[3])
							} else if idx >= points.len() - 1 {
								(&points[idx - 4], &points[idx - 3], &points[idx - 2], &points[idx - 1])
							} else {
								(&points[idx - 2], &points[idx - 1], &points[idx], &points[idx + 1])
							};
							let value = cubic_interpolate(p0, p1, p2, p3, time);
							let point = Point { timestamp: time, value };
							let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?}"), resolution: format!("{resolution:?}") };
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
						Spline::Polynomial(degree, _) => {
							// Need degree+1 points for polynomial
							if points.len() < degree + 1 {
								bail!("Insufficient measurements for polynomial interpolation of degree {}", degree);
							}
							// For now, fall back to linear interpolation
							let value = interpolate_linear(&points[idx - 1], &points[idx], time);
							let point = Point { timestamp: time, value };
							let analysis_result = AnalysisResult { timestamp: time, value: point.value.clone(), method: format!("{method:?} (fallback to linear)"), resolution: format!("{resolution:?}") };
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
					}
				}
			}
		}
	}

	/// Interpolates/extrapolates the `DataPoint`[] for a given time range, resolution, & spline type.
	/// Uses intelligent measurement collection and caching with GPU acceleration when beneficial.
	///
	/// # Errors
	/// - if interpolation fails
	pub async fn analyze_range(aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Vec<Point>> {
		// Calculate total duration and step in nanoseconds for precision
		let total_duration = end - start;
		let step_duration = resolution.to_step();

		// Calculate expected number of points
		let total_ns = total_duration.num_nanoseconds().unwrap_or(i64::MAX);
		let step_ns = step_duration.num_nanoseconds().unwrap_or(1);
		let expected = if step_ns > 0 { (total_ns / step_ns) + 1 } else { 1 };

		if expected > 1_000_000 {
			let chunk_size = 100_000;
			let chunk_count = (expected + chunk_size as i64 - 1) / chunk_size as i64;
			let chunk_duration_ns = total_ns / chunk_count;
			let chunk_duration = chrono::Duration::nanoseconds(chunk_duration_ns);

			let overlap = match method {
				Spline::Linear => 1,
				Spline::Quadratic => 2,
				Spline::Cubic => 3,
				Spline::Polynomial(d, _) => d as u64,
			};
			let overlap_duration = chrono::Duration::nanoseconds(overlap as i64 * step_ns);

			let mut all_points = Vec::with_capacity(expected as usize);
			let mut current = start;

			while current < end {
				let chunk_end = current + chunk_duration;
				let fetch_start = (current - overlap_duration).max(start);
				let fetch_end = (chunk_end + overlap_duration).min(end);

				let chunk_measurements = Self::get_aspect_measurements_range(aspect, fetch_start, fetch_end).await?;
				let mut chunk_points = Self::measurements_to_points(&chunk_measurements);
				let interpolated = splimes::auto_interpolate(&mut chunk_points, current, chunk_end.min(end), resolution, method).await?;

				all_points.extend(interpolated);

				current = chunk_end;
			}
			Ok(all_points)
		} else {
			let measurements = Self::get_aspect_measurements_range(aspect, start, end).await?;

			if measurements.is_empty() {
				bail!("No measurements found for aspect");
			}

			splimes::auto_interpolate(&mut Self::measurements_to_points(&measurements), start, end, resolution, method).await
		}
	}

	/// Interpolates/extrapolates the `DataPoint`[] for a given time range, resolution, & spline type.
	/// Uses intelligent measurement collection and caching with GPU acceleration when beneficial.
	///
	/// # Errors
	/// - if interpolation fails
	pub fn stream_analyze_range(&self, aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>> {
		let total_duration = end - start;
		let step_duration = resolution.to_step();

		// Calculate expected number of points
		let total_ns = total_duration.num_nanoseconds().unwrap_or(i64::MAX);
		let step_ns = step_duration.num_nanoseconds().unwrap_or(1);
		let expected = if step_ns > 0 { (total_ns / step_ns) + 1 } else { 1 };

		let chunk_size = 100_000;
		let chunk_count = if expected > 0 { (expected + chunk_size as i64 - 1) / chunk_size as i64 } else { 1 };
		let chunk_duration_ns = total_ns / chunk_count;
		let chunk_duration = chrono::Duration::nanoseconds(chunk_duration_ns);

		let overlap = match method {
			Spline::Linear => 1,
			Spline::Quadratic => 2,
			Spline::Cubic => 3,
			Spline::Polynomial(d, _) => d as u64,
		};
		let overlap_duration = chrono::Duration::nanoseconds(overlap as i64 * step_ns);

		Box::pin(futures::stream::unfold((start, Vec::<Point>::new().into_iter(), false), move |(mut current, mut point_iter, done)| {
			let aspect = aspect;
			let end = end;
			let resolution = resolution;
			let method = method;
			let overlap_duration = overlap_duration;
			let chunk_duration = chunk_duration;

			async move {
				// Return next point from current chunk if available
				if let Some(point) = point_iter.next() {
					return Some((Ok(point), (current, point_iter, done)));
				}

				// If we're done or current is past end, return None
				if current >= end || done {
					return None;
				}

				// Process next chunk
				let chunk_end = (current + chunk_duration).min(end);
				let fetch_start = (current - overlap_duration).max(start);
				let fetch_end = (chunk_end + overlap_duration).min(end);

				match Self::get_aspect_measurements_range(aspect, fetch_start, fetch_end).await {
					Ok(chunk_measurements) => {
						let mut chunk_points = Self::measurements_to_points(&chunk_measurements);
						match splimes::auto_interpolate(&mut chunk_points, current, chunk_end, resolution, method).await {
							Ok(interpolated) => {
								current = chunk_end;
								let mut new_iter = interpolated.into_iter();
								// Return first point and set up iterator for the rest
								if let Some(first_point) = new_iter.next() {
									Some((Ok(first_point), (current, new_iter, current >= end)))
								} else {
									// If no points, continue to next chunk
									Some((Ok(Point::new(current, BigDecimal::zero())), (current, Vec::<Point>::new().into_iter(), current >= end)))
								}
							}
							Err(e) => Some((Err(e), (current, Vec::<Point>::new().into_iter(), true))),
						}
					}
					Err(e) => Some((Err(e), (current, Vec::<Point>::new().into_iter(), true))),
				}
			}
		}))
	}
}

// Helper functions
fn interpolate_linear(p1: &Point, p2: &Point, time: DateTime<Utc>) -> BigDecimal {
	let t1 = p1.timestamp.timestamp_nanos_opt().unwrap_or(p1.timestamp.timestamp() * 1_000_000_000) as f64;
	let t2 = p2.timestamp.timestamp_nanos_opt().unwrap_or(p2.timestamp.timestamp() * 1_000_000_000) as f64;
	let t = time.timestamp_nanos_opt().unwrap_or(time.timestamp() * 1_000_000_000) as f64;

	let v1 = p1.value.to_f64().unwrap();
	let v2 = p2.value.to_f64().unwrap();

	let frac = (t - t1) / (t2 - t1);
	let v = v1 + frac * (v2 - v1);
	BigDecimal::from_f64(v).unwrap()
}

fn extrapolate_linear(p1: &Point, p2: &Point, time: DateTime<Utc>) -> BigDecimal {
	interpolate_linear(p1, p2, time) // Same as interpolation for linear
}

fn quadratic_interpolate(p1: &Point, p2: &Point, p3: &Point, time: DateTime<Utc>) -> BigDecimal {
	// Lagrange interpolation for three points
	let t1 = p1.timestamp.timestamp_nanos_opt().unwrap_or(p1.timestamp.timestamp() * 1_000_000_000) as f64;
	let t2 = p2.timestamp.timestamp_nanos_opt().unwrap_or(p2.timestamp.timestamp() * 1_000_000_000) as f64;
	let t3 = p3.timestamp.timestamp_nanos_opt().unwrap_or(p3.timestamp.timestamp() * 1_000_000_000) as f64;
	let t = time.timestamp_nanos_opt().unwrap_or(time.timestamp() * 1_000_000_000) as f64;

	let v1 = p1.value.to_f64().unwrap();
	let v2 = p2.value.to_f64().unwrap();
	let v3 = p3.value.to_f64().unwrap();

	let l1 = ((t - t2) * (t - t3)) / ((t1 - t2) * (t1 - t3)) * v1;
	let l2 = ((t - t1) * (t - t3)) / ((t2 - t1) * (t2 - t3)) * v2;
	let l3 = ((t - t1) * (t - t2)) / ((t3 - t1) * (t3 - t2)) * v3;

	BigDecimal::from_f64(l1 + l2 + l3).unwrap()
}

fn cubic_interpolate(p0: &Point, p1: &Point, p2: &Point, p3: &Point, time: DateTime<Utc>) -> BigDecimal {
	// Lagrange interpolation for four points
	let t0 = p0.timestamp.timestamp_nanos_opt().unwrap_or(p0.timestamp.timestamp() * 1_000_000_000) as f64;
	let t1 = p1.timestamp.timestamp_nanos_opt().unwrap_or(p1.timestamp.timestamp() * 1_000_000_000) as f64;
	let t2 = p2.timestamp.timestamp_nanos_opt().unwrap_or(p2.timestamp.timestamp() * 1_000_000_000) as f64;
	let t3 = p3.timestamp.timestamp_nanos_opt().unwrap_or(p3.timestamp.timestamp() * 1_000_000_000) as f64;
	let t = time.timestamp_nanos_opt().unwrap_or(time.timestamp() * 1_000_000_000) as f64;

	let v0 = p0.value.to_f64().unwrap();
	let v1 = p1.value.to_f64().unwrap();
	let v2 = p2.value.to_f64().unwrap();
	let v3 = p3.value.to_f64().unwrap();

	let l0 = ((t - t1) * (t - t2) * (t - t3)) / ((t0 - t1) * (t0 - t2) * (t0 - t3)) * v0;
	let l1 = ((t - t0) * (t - t2) * (t - t3)) / ((t1 - t0) * (t1 - t2) * (t1 - t3)) * v1;
	let l2 = ((t - t0) * (t - t1) * (t - t3)) / ((t2 - t0) * (t2 - t1) * (t2 - t3)) * v2;
	let l3 = ((t - t0) * (t - t1) * (t - t2)) / ((t3 - t0) * (t3 - t1) * (t3 - t2)) * v3;

	BigDecimal::from_f64(l0 + l1 + l2 + l3).unwrap()
}
