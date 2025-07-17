#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Spline {
    Linear,
    Quadratic,
    Cubic,
    Polynomial(usize, Option<f64>), // (degree, bounds_factor)
}

impl Spline {
    #[must_use]
    pub const fn degree(&self) -> usize {
        match self {
            Self::Linear => 1,
            Self::Quadratic => 2,
            Self::Cubic => 3,
            Self::Polynomial(degree, _) => *degree,
        }
    }

    #[must_use]
    pub const fn number_of_points_required(&self) -> usize {
        match self {
            Self::Linear => 2,
            Self::Quadratic => 3,
            Self::Cubic => 4,
            Self::Polynomial(degree, _) => *degree + 1, // Degree n requires n+1 points
        }
    }

    #[must_use]
    pub const fn bounds_factor(&self) -> Option<f64> {
        match self {
            Self::Linear | Self::Quadratic | Self::Cubic => None, // These don't support bounds
            Self::Polynomial(_, bounds) => *bounds,
        }
    }
}
