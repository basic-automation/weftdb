use chrono::{DateTime, Duration, Utc};

use crate::Resolution;

/// Generate target times for interpolation
pub fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
	let mut target_times = Vec::new();
	let mut current = start;

	let step = match resolution {
		Resolution::Nanoseconds => Duration::nanoseconds(1),
		Resolution::Microseconds => Duration::microseconds(1),
		Resolution::Milliseconds => Duration::milliseconds(1),
		Resolution::Seconds => Duration::seconds(1),
		Resolution::Minutes => Duration::minutes(1),
		Resolution::Hours => Duration::hours(1),
		Resolution::Days => Duration::days(1),
		Resolution::Weeks => Duration::weeks(1),
		Resolution::Months => Duration::days(30), // Approximate
		Resolution::Years => Duration::days(365), // Approximate
	};

	while current <= end {
		target_times.push(current);
		current += step;
	}

	target_times
}
