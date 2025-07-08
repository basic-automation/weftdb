use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{TimeZone, Utc};
use fake::Dummy;
use rand::Rng;
use uuid::Uuid;

use crate::InputMeasurement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub timestamp: chrono::DateTime<Utc>,
    pub value: BigDecimal,
}

impl Measurement {
    /// Creates a new measurement from an input measurement with a generated ID
    #[must_use]
    pub fn from_input_measurement(dataset_id: Uuid, input: &InputMeasurement) -> Self {
        Self { id: Uuid::new_v4(), dataset_id, timestamp: input.timestamp(), value: input.value().clone() }
    }

    /// Generate fake measurement data for testing
    #[must_use]
    pub fn fake(dataset_id: Uuid) -> Self {
        let mut rng = rand::thread_rng();
        Self { 
            id: Uuid::new_v4(), 
            dataset_id, 
            timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.gen_range(0..86400)), 
            value: BigDecimal::from_f64(rng.gen_range(0.0..100.0)).unwrap_or_default() 
        }
    }

    #[must_use]
    pub const fn new(id: Uuid, dataset_id: Uuid, timestamp: chrono::DateTime<Utc>, value: BigDecimal) -> Self {
        Self { id, dataset_id, timestamp, value }
    }

    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    #[must_use]
    pub const fn dataset_id(&self) -> Uuid {
        self.dataset_id
    }

    #[must_use]
    pub const fn timestamp(&self) -> chrono::DateTime<Utc> {
        self.timestamp
    }

    #[must_use]
    pub const fn value(&self) -> &BigDecimal {
        &self.value
    }

    pub const fn set_id(&mut self, id: Uuid) {
        self.id = id;
    }

    pub const fn set_dataset_id(&mut self, dataset_id: Uuid) {
        self.dataset_id = dataset_id;
    }

    pub const fn set_timestamp(&mut self, timestamp: chrono::DateTime<Utc>) {
        self.timestamp = timestamp;
    }

    pub fn set_value(&mut self, value: BigDecimal) {
        self.value = value;
    }

    /// Converts the measurement to an input measurement
    #[must_use]
    pub fn to_input_measurement(&self) -> InputMeasurement {
        InputMeasurement::new(self.timestamp, self.value.clone())
    }
}

impl<T> Dummy<T> for Measurement {
    fn dummy_with_rng<R: Rng + ?Sized>(_config: &T, rng: &mut R) -> Self {
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.gen_range(0..86400));
        let value = BigDecimal::from_f64(rng.gen_range(0.0..100.0)).unwrap_or_else(|| BigDecimal::from(50));
        
        Self {
            id: Uuid::new_v4(),
            dataset_id: Uuid::new_v4(),
            timestamp,
            value,
        }
    }
}
