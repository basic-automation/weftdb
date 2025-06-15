use rayon::prelude::*;

pub fn auto_interpolate_parallel(
    measurements: Vec<Measurement>, 
    start: DateTime<Utc>, 
    end: DateTime<Utc>, 
    resolution: Resolution, 
    spline_type: SplineType
) -> Result<Vec<Measurement>> {
    let num_points = measurements.len();
    
    // Use parallel processing for large datasets
    if num_points > 1000 {
        return interpolate_chunked_parallel(measurements, start, end, resolution, spline_type);
    }
    
    // Use regular implementation for smaller datasets
    auto_interpolate(measurements, start, end, resolution, spline_type)
}

fn interpolate_chunked_parallel(
    measurements: Vec<Measurement>, 
    start: DateTime<Utc>, 
    end: DateTime<Utc>, 
    resolution: Resolution, 
    spline_type: SplineType
) -> Result<Vec<Measurement>> {
    let chunk_size = 500;
    let step_duration = resolution.to_chrono_duration();
    let total_duration = end - start;
    let chunk_duration = chrono::Duration::milliseconds(
        (total_duration.num_milliseconds() * chunk_size as i64) / 
        ((total_duration.num_milliseconds() / step_duration.num_milliseconds()) as i64)
    );
    
    let mut chunk_ranges = Vec::new();
    let mut current_start = start;
    
    while current_start < end {
        let chunk_end = (current_start + chunk_duration).min(end);
        chunk_ranges.push((current_start, chunk_end));
        current_start = chunk_end;
    }
    
    // Process chunks in parallel
    let results: Result<Vec<Vec<Measurement>>, _> = chunk_ranges
        .into_par_iter()
        .map(|(chunk_start, chunk_end)| {
            let relevant_measurements: Vec<Measurement> = measurements.iter()
                .filter(|m| m.timestamp >= chunk_start - step_duration && 
                           m.timestamp <= chunk_end + step_duration)
                .cloned()
                .collect();
            
            auto_interpolate(relevant_measurements, chunk_start, chunk_end, resolution, spline_type)
        })
        .collect();
    
    // Combine results
    let mut combined = Vec::new();
    for chunk_result in results? {
        combined.extend(chunk_result);
    }
    
    Ok(combined)
}