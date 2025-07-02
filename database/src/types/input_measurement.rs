use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{TimeZone, Utc};
use fake::{Dummy, Faker};
use rand::Rng;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMeasurement {
	timestamp: chrono::DateTime<Utc>,
	value: BigDecimal,
}

impl InputMeasurement {
	/// Generate fake input measurement data for testing
	#[must_use]
	pub fn fake() -> Self {
		let mut rng = rand::rng();
		Self { timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.random_range(0..86400)), value: BigDecimal::from_f64(rng.random_range(0.0..100.0)).unwrap_or_default() }
	}

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
}

impl Dummy<Faker> for InputMeasurement {
	fn dummy_with_rng<R: rand::Rng + ?Sized>(_config: &Faker, rng: &mut R) -> Self {
		use fake::{faker::number::en::NumberWithFormat, Fake};

		let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.random_range(0..86400));

		let value_str: String = NumberWithFormat("##.##").fake();
		let value = BigDecimal::parse_bytes(value_str.as_bytes(), 10).unwrap_or_else(|| BigDecimal::from(0));

		Self { timestamp, value }
	}
}
