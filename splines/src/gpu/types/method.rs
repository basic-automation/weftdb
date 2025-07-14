use crate::gpu::shaders::{CUBIC_INTERPOLATION_SHADER, LINEAR_INTERPOLATION_SHADER, POLYNOMIAL_INTERPOLATION_SHADER, QUADRATIC_INTERPOLATION_SHADER};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
	Linear,
	Quadratic,
	Cubic,
	Polynomial(usize), // Degree of polynomial
}

impl Method {
	pub const fn shader_source(self) -> &'static str {
		match self {
			Self::Linear => LINEAR_INTERPOLATION_SHADER,
			Self::Quadratic => QUADRATIC_INTERPOLATION_SHADER,
			Self::Cubic => CUBIC_INTERPOLATION_SHADER,
			Self::Polynomial(degree) if degree > 3 => {
				// For polynomials of degree > 3, use the polynomial shader
				POLYNOMIAL_INTERPOLATION_SHADER
			}
			// For degrees 0, 1, 2, or 3, us the appropriate shader
			Self::Polynomial(degree) if degree == 0 => LINEAR_INTERPOLATION_SHADER,
			Self::Polynomial(degree) if degree == 1 => LINEAR_INTERPOLATION_SHADER,
			Self::Polynomial(degree) if degree == 2 => QUADRATIC_INTERPOLATION_SHADER,
			Self::Polynomial(degree) if degree == 3 => CUBIC_INTERPOLATION_SHADER,
			// Fallback for any other case
			// This should not happen, but if it does, use linear interpolation
			_ => LINEAR_INTERPOLATION_SHADER,
		}
	}
}
