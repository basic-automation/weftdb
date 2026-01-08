use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{TimeZone, Utc};
use fake::Fake;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMeasurement {
	timestamp: chrono::DateTime<Utc>,
	value: BigDecimal,
}

impl InputMeasurement {
	#[must_use]
	pub const fn new(timestamp: chrono::DateTime<Utc>, value: BigDecimal) -> Self {
		Self { timestamp, value }
	}

	#[must_use]
	pub const fn timestamp(&self) -> chrono::DateTime<Utc> {
		self.timestamp
	}

	#[must_use]
	pub const fn value(&self) -> &BigDecimal {
		&self.value
	}

	pub const fn set_timestamp(&mut self, timestamp: chrono::DateTime<Utc>) {
		self.timestamp = timestamp;
	}

	pub fn set_value(&mut self, value: BigDecimal) {
		self.value = value;
	}

	/// Generate a random `InputMeasurement` for testing
	#[must_use]
	pub fn random() -> Self {
		let offset_secs: i64 = fake::Faker.fake();
		let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(offset_secs.rem_euclid(86400));
		let value_f64: f64 = fake::Faker.fake();
		let value = BigDecimal::from_f64(value_f64.rem_euclid(100.0)).unwrap_or_else(|| BigDecimal::from(50));

		Self { timestamp, value }
	}
}
