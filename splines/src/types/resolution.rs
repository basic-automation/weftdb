#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
	Nanoseconds,
	Microseconds,
	Milliseconds,
	Seconds,
	Minutes,
	Hours,
	Days,
	Weeks,
	Months,
	Years,
}

impl Resolution {
	#[must_use]
	pub const fn to_step(self) -> chrono::Duration {
		match self {
			Self::Nanoseconds => chrono::Duration::nanoseconds(1),
			Self::Microseconds => chrono::Duration::microseconds(1),
			Self::Milliseconds => chrono::Duration::milliseconds(1),
			Self::Seconds => chrono::Duration::seconds(1),
			Self::Minutes => chrono::Duration::minutes(1),
			Self::Hours => chrono::Duration::hours(1),
			Self::Days => chrono::Duration::days(1),
			Self::Weeks => chrono::Duration::weeks(1),
			Self::Months => chrono::Duration::days(30), // Approximate 30-day month
			Self::Years => chrono::Duration::days(365), // Approximate 365-day year
		}
	}
}
