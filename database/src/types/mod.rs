use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{TimeZone, Utc};
use fake::{Dummy, Faker};
use rand::Rng;
use uuid::Uuid;

mod dataset;
mod error;
mod measurement;

pub use error::*;

#[derive(Debug, Clone)]
pub struct Dataset {
    pub id: Uuid,
    pub name: String,
    pub measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub value: BigDecimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMeasurement {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub value: BigDecimal,
}

impl Measurement {
    /// Creates a new measurement from an input measurement with a generated ID
    #[must_use]
    pub fn from_input_measurement(dataset_id: Uuid, input: InputMeasurement) -> Self {
        Self {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: input.timestamp,
            value: input.value,
        }
    }

    /// Generate fake measurement data for testing
    #[must_use]
    pub fn fake(dataset_id: Uuid) -> Self {
        let mut rng = rand::rng();
        Self {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.random_range(0..86400)),
            value: BigDecimal::from_f64(rng.random_range(0.0..100.0)).unwrap_or_default(),
        }
    }
}

impl InputMeasurement {
    /// Generate fake input measurement data for testing
    #[must_use]
    pub fn fake() -> Self {
        let mut rng = rand::rng();
        Self {
            timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.random_range(0..86400)),
            value: BigDecimal::from_f64(rng.random_range(0.0..100.0)).unwrap_or_default(),
        }
    }
}

// Implement Dummy manually for the types that need it
impl Dummy<Faker> for Dataset {
    fn dummy_with_rng<R: rand::Rng + ?Sized>(_config: &Faker, _rng: &mut R) -> Self {
        use fake::{faker::lorem::en::Word, Fake};

        Self {
            id: Uuid::new_v4(),
            name: Word().fake(),
            measurements: vec![], // Start with empty measurements
        }
    }
}

impl Dummy<Faker> for Measurement {
    fn dummy_with_rng<R: rand::Rng + ?Sized>(_config: &Faker, rng: &mut R) -> Self {
        use fake::{faker::number::en::NumberWithFormat, Fake};

        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.random_range(0..86400));

        let value_str: String = NumberWithFormat("##.##").fake();
        let value = BigDecimal::parse_bytes(value_str.as_bytes(), 10).unwrap_or_else(|| BigDecimal::from(0));

        Self { id: Uuid::new_v4(), dataset_id: Uuid::new_v4(), timestamp, value }
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
