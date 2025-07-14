use chrono::Duration;

use crate::{Point, Resolution};

/// Check if measurements are uniformly spaced
pub fn is_uniformly_spaced(points: &[Point], resolution: Resolution) -> bool {
	if points.len() < 3 {
		return false;
	}

	let first_interval = points[1].timestamp - points[0].timestamp;

	// If the first interval is zero, this is not uniformly spaced (it's constant time)
	match resolution {
		Resolution::Nanoseconds => {
			if first_interval.num_nanoseconds().is_none() {
				return false;
			}
		}
		Resolution::Microseconds => {
			if first_interval.num_microseconds().is_none() {
				return false;
			}
		}
		Resolution::Milliseconds => {
			if first_interval.num_milliseconds() == 0 {
				return false;
			}
		}
		Resolution::Seconds => {
			if first_interval.num_seconds() == 0 {
				return false;
			}
		}
		Resolution::Minutes => {
			if first_interval.num_minutes() == 0 {
				return false;
			}
		}
		Resolution::Hours => {
			if first_interval.num_hours() == 0 {
				return false;
			}
		}
		Resolution::Days => {
			if first_interval.num_days() == 0 {
				return false;
			}
		}
		Resolution::Weeks => {
			if first_interval.num_weeks() == 0 {
				return false;
			}
		}
		Resolution::Months => {
			// Approximate month as 30 days
			if first_interval.num_days() % 30 != 0 {
				return false;
			}
		}
		Resolution::Years => {
			// Approximate year as 365 days
			if first_interval.num_days() % 365 != 0 {
				return false;
			}
		}
	}

	// Set tolerance based on resolution
	let tolerance = match resolution {
		Resolution::Nanoseconds => Duration::nanoseconds(1000),  // 1 microsecond tolerance
		Resolution::Microseconds => Duration::microseconds(100), // 100 microsecond tolerance
		Resolution::Milliseconds => Duration::milliseconds(50),  // 50 millisecond tolerance
		Resolution::Seconds => Duration::seconds(1),             // 1 second tolerance
		Resolution::Minutes => Duration::minutes(1),             // 1 minute tolerance
		Resolution::Hours => Duration::hours(1),                 // 1 hour tolerance
		Resolution::Days => Duration::days(1),                   // 1 day tolerance
		Resolution::Weeks => Duration::weeks(1),                 // 1 week tolerance
		Resolution::Months => Duration::days(30),                // ~1 month tolerance
		Resolution::Years => Duration::days(365),                // ~1 year tolerance
	};

	points.windows(2).all(|pair| {
		let interval = pair[1].timestamp - pair[0].timestamp;
		(interval - first_interval).abs() < tolerance
	})
}
