use crate::types::Spline;

/// Apply fast path optimization to spline types based on dataset characteristics
///
/// This function automatically downgrades complex spline types to simpler ones
/// for better performance when dealing with large datasets or specific conditions.
///
/// # Arguments
/// * `spline_type` - The requested spline type
/// * `measurement_count` - Number of input measurements
///
/// # Returns
/// Optimized spline type that provides better performance characteristics
#[must_use]
pub const fn apply_fast_path(spline: Spline, measurement_count: usize) -> Spline {
	match spline {
		// Linear always stays linear - it's already optimal
		Spline::Linear => Spline::Linear,

		// Quadratic optimizations
		Spline::Quadratic => {
			if measurement_count > 5000 {
				// Very large datasets: degrade to linear for speed
				Spline::Linear
			} else {
				Spline::Quadratic
			}
		}

		// Cubic optimizations
		Spline::Cubic => {
			if measurement_count > 5000 {
				// Very large datasets: degrade to linear
				Spline::Linear
			} else if measurement_count >= 2500 {
				// Large datasets: degrade to quadratic
				Spline::Quadratic
			} else {
				Spline::Cubic
			}
		}

		// Polynomial optimizations
		Spline::Polynomial(degree) => {
			if degree > 5 || measurement_count > 1000 {
				// High degree or large datasets: degrade to quadratic
				Spline::Quadratic
			} else if measurement_count > 3000 {
				// Very large datasets: degrade to linear
				Spline::Linear
			} else if degree <= 2 {
				// Low degree: use quadratic
				Spline::Quadratic
			} else if degree == 3 {
				// Degree 3: use cubic
				Spline::Cubic
			} else {
				// Moderate degree: keep as polynomial but limit degree
				Spline::Polynomial(degree)
			}
		}
	}
}
