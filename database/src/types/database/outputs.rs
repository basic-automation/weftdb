use std::pin::Pin;

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use futures::Stream;
use splimes::{Point, Resolution, Spline};

use crate::{database::traits::Outputs, types::cache::CACHE, AnalysisResult, AspectId, Database};

#[async_trait::async_trait]
impl Outputs for Database {
	/// Analyze a single point in time for an aspect using interpolation
	///
	/// # Errors
	/// - if aspect not found
	/// - if unable to retrieve measurements
	/// - if interpolation fails
	async fn analyze_point(&self, aspect: AspectId, time: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Point> {
		// Check cache first for point analysis
		let cache_key = format!("point_{}_{}_{}_{:?}_{:?}", aspect.as_uuid(), time.timestamp(), time.timestamp_subsec_nanos(), resolution, method);
		if let Some(cached_result) = CACHE.get_point_analysis(&cache_key).await {
			return Ok(Point { timestamp: cached_result.timestamp(), value: cached_result.value().clone() });
		}

		// Get measurements for this aspect, sorted by timestamp
		let mut measurements = Self::get_aspect_measurements(aspect).await?;

		if measurements.is_empty() {
			bail!("No measurements found for aspect");
		}

		// Sort measurements by timestamp (in case they are not sorted)
		measurements.sort_by_key(super::super::measurement::Measurement::timestamp);

		// Convert to Points
		let points: Vec<Point> = measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

		// Find the position where time would fit
		let pos = points.binary_search_by_key(&time, |p| p.timestamp);

		match pos {
			Ok(idx) => {
				// Exact match found
				let point = points[idx].clone();
				let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?}"), format!("{resolution:?}"));
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
					let mut point_slice = points.clone();
					let end_time = time + resolution.to_step();
					let interpolated = splimes::auto_interpolate(&mut point_slice, time, end_time, resolution, method).await?;
					let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });
					let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?} (backward extrapolation)"), format!("{resolution:?}"));
					CACHE.store_point_analysis(&cache_key, &analysis_result).await;
					Ok(point)
				} else if idx >= points.len() {
					// After last measurement - forward extrapolation
					if points.len() < 2 {
						bail!("Insufficient measurements for extrapolation");
					}
					let mut point_slice = points.clone();
					let end_time = time + resolution.to_step();
					let interpolated = splimes::auto_interpolate(&mut point_slice, time, end_time, resolution, method).await?;
					let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });
					let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?} (forward extrapolation)"), format!("{resolution:?}"));
					CACHE.store_point_analysis(&cache_key, &analysis_result).await;
					Ok(point)
				} else {
					// Interpolation between measurements
					match method {
						Spline::Linear => {
							// Linear interpolation between points[idx-1] and points[idx]
							let mut point_slice = points.clone();
							let end_time = time + resolution.to_step();
							let interpolated = splimes::auto_interpolate(&mut point_slice, time, end_time, resolution, method).await?;
							let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });
							let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?}"), format!("{resolution:?}"));
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
						Spline::Quadratic => {
							// Need 3 points for quadratic
							if points.len() < 3 {
								bail!("Insufficient measurements for quadratic interpolation");
							}
							let mut point_slice = points.clone();
							let end_time = time + resolution.to_step();
							let interpolated = splimes::auto_interpolate(&mut point_slice, time, end_time, resolution, method).await?;
							let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });
							let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?}"), format!("{resolution:?}"));
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
						Spline::Cubic => {
							// Need 4 points for cubic
							if points.len() < 4 {
								bail!("Insufficient measurements for cubic interpolation");
							}
							let mut point_slice = points.clone();
							let end_time = time + resolution.to_step();
							let interpolated = splimes::auto_interpolate(&mut point_slice, time, end_time, resolution, method).await?;
							let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });
							let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?}"), format!("{resolution:?}"));
							CACHE.store_point_analysis(&cache_key, &analysis_result).await;
							Ok(point)
						}
						Spline::Polynomial(degree, _) => {
							// Need degree+1 points for polynomial
							if points.len() < degree + 1 {
								bail!("Insufficient measurements for polynomial interpolation of degree {}", degree);
							}
							let mut point_slice = points.clone();
							let end_time = time + resolution.to_step();
							let interpolated = splimes::auto_interpolate(&mut point_slice, time, end_time, resolution, method).await?;
							let point = interpolated.into_iter().next().unwrap_or_else(|| Point { timestamp: time, value: BigDecimal::zero() });
							let analysis_result = AnalysisResult::new(time, point.value.clone(), format!("{method:?} (fallback to linear)"), format!("{resolution:?}"));
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
	async fn analyze_range(&self, aspect: AspectId, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, method: Spline) -> Result<Pin<Box<dyn Stream<Item = Result<Point>> + Send + 'static>>> {
		let total_duration = end - start;
		let step_duration = resolution.to_step();

		// Calculate expected number of points
		let total_ns = total_duration.num_nanoseconds().unwrap_or(i64::MAX);
		let step_ns = step_duration.num_nanoseconds().unwrap_or(1);
		let expected = if step_ns > 0 { (total_ns / step_ns) + 1 } else { 1 };

		let chunk_size = 100_000;
		let chunk_count = if expected > 0 { (expected + i64::from(chunk_size) - 1) / i64::from(chunk_size) } else { 1 };
		let chunk_duration_ns = total_ns / chunk_count;
		let chunk_duration = chrono::Duration::nanoseconds(chunk_duration_ns);

		let overlap = match method {
			Spline::Linear => 1,
			Spline::Quadratic => 2,
			Spline::Cubic => 3,
			Spline::Polynomial(d, _) => d as u64,
		};
		let overlap_ns = i64::try_from(overlap).unwrap_or(i64::MAX);
		let overlap_duration = chrono::Duration::nanoseconds(overlap_ns.saturating_mul(step_ns));
		let aspect_id = aspect;
		let end_time = end;
		let resolution_value = resolution;
		let method_choice = method;
		let overlap_duration_closure = overlap_duration;
		let chunk_duration_closure = chunk_duration;
		let range_start = start;

		Ok(Box::pin(futures::stream::unfold((start, Vec::<Point>::new().into_iter(), false, range_start), move |(mut current, mut point_iter, done, range_start)| {
			let aspect = aspect_id;
			let end = end_time;
			let resolution = resolution_value;
			let method = method_choice;
			let overlap_duration = overlap_duration_closure;
			let chunk_duration = chunk_duration_closure;

			async move {
				// Return next point from current chunk if available
				if let Some(point) = point_iter.next() {
					return Some((Ok(point), (current, point_iter, done, range_start)));
				}

				// If we're done or current is past end, return None
				if current >= end || done {
					return None;
				}

				// Process next chunk
				let chunk_end = (current + chunk_duration).min(end);
				let fetch_start = (current - overlap_duration).max(range_start);
				let fetch_end = (chunk_end + overlap_duration).min(end);

				match Self::get_aspect_measurements_range(aspect, fetch_start, fetch_end).await {
					Ok(chunk_measurements) => {
						let mut chunk_points = Self::measurements_to_points(&chunk_measurements);
						match splimes::auto_interpolate(&mut chunk_points, current, chunk_end, resolution, method).await {
							Ok(interpolated) => {
								current = chunk_end;
								let mut new_iter = interpolated.into_iter();
								// Return first point and set up iterator for the rest
								match new_iter.next() {
									Some(first_point) => Some((Ok(first_point), (current, new_iter, current >= end, range_start))),
									None => Some((Ok(Point { timestamp: current, value: BigDecimal::zero() }), (current, new_iter, current >= end, range_start))),
								}
							}
							Err(e) => Some((Err(e), (current, Vec::<Point>::new().into_iter(), true, range_start))),
						}
					}
					Err(e) => Some((Err(e), (current, Vec::<Point>::new().into_iter(), true, range_start))),
				}
			}
		})))
	}
}
