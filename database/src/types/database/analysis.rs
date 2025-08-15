use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
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
		// Get measurements for this aspect
		let measurements = Self::get_aspect_measurements(aspect).await?;

		if measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Use the GPU-aware auto_interpolate function
		splimes::auto_interpolate(&mut Self::measurements_to_points(&measurements), start, end, resolution, method).await
	}
}

// Helper functions
fn interpolate_linear(p1: &Point, p2: &Point, time: DateTime<Utc>) -> BigDecimal {
	let t1 = p1.timestamp.timestamp() as f64;
	let t2 = p2.timestamp.timestamp() as f64;
	let t = time.timestamp() as f64;

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
	let t1 = p1.timestamp.timestamp() as f64;
	let t2 = p2.timestamp.timestamp() as f64;
	let t3 = p3.timestamp.timestamp() as f64;
	let t = time.timestamp() as f64;

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
	let t0 = p0.timestamp.timestamp() as f64;
	let t1 = p1.timestamp.timestamp() as f64;
	let t2 = p2.timestamp.timestamp() as f64;
	let t3 = p3.timestamp.timestamp() as f64;
	let t = time.timestamp() as f64;

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
