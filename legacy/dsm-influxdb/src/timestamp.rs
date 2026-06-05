use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub enum InfluxTimestamp {
	DateTime(DateTime<Utc>),
	Unix(i64),
	Now,
}

#[derive(Debug, Clone)]
pub enum InfluxTimestampPrecision {
	Nanoseconds,
	Microseconds,
	Milliseconds,
	Seconds,
}
