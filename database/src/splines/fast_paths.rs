// Add specialized implementations for common cases
pub fn auto_interpolate_with_fast_paths(
    measurements: Vec<Measurement>, 
    start: DateTime<Utc>, 
    end: DateTime<Utc>, 
    resolution: Resolution, 
    spline_type: SplineType
) -> Result<Vec<Measurement>> {
    // Fast path: uniform spacing with linear interpolation
    if spline_type == SplineType::Linear && is_uniformly_spaced(&measurements) {
        return linear_interpolate_uniform(measurements, start, end, resolution);
    }
    
    // Fast path: small time ranges with few points
    let duration_seconds = (end - start).num_seconds();
    if duration_seconds <= 60 && measurements.len() <= 10 {
        return interpolate_small_range(measurements, start, end, resolution, spline_type);
    }
    
    // Fast path: exact timestamp matches (no interpolation needed)
    if let Some(exact_matches) = check_exact_matches(&measurements, start, end, resolution) {
        return Ok(exact_matches);
    }
    
    // Use regular implementation
    auto_interpolate(measurements, start, end, resolution, spline_type)
}

fn is_uniformly_spaced(measurements: &[Measurement]) -> bool {
    if measurements.len() < 3 { return false; }
    
    let first_interval = measurements[1].timestamp - measurements[0].timestamp;
    measurements.windows(2).all(|pair| {
        (pair[1].timestamp - pair[0].timestamp - first_interval).num_milliseconds().abs() < 100
    })
}