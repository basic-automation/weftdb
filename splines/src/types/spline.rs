#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spline {
	Linear,
	Quadratic,
	Cubic,
	Polynomial(usize),
}

impl Spline {
	#[must_use]
	pub const fn degree(&self) -> usize {
		match self {
			Self::Linear => 1,
			Self::Quadratic => 2,
			Self::Cubic => 3,
			Self::Polynomial(degree) => *degree,
		}
	}

	#[must_use]
	pub const fn number_of_points_required(&self) -> usize {
		match self {
			Self::Linear => 2,
			Self::Quadratic => 3,
			Self::Cubic => 4,
			Self::Polynomial(degree) => *degree + 1, // Degree n requires n+1 points
		}
	}
}
