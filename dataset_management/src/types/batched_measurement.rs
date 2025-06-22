use database::Measurement;
use crate::types::{
    Distance,
    MeasurementVector,
    Analysis,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedMeasurement {
    active: bool,
    measurement: Measurement,
    distance: Option<Distance>,
    vector: Option<MeasurementVector>,
    analysis: Option<Analysis>,
}

impl BatchedMeasurement {
    pub fn new(measurement: Measurement) -> Self {
        Self {
            active: true,
            measurement,
            distance: None,
            vector: None,
            analysis: None,
        }
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn activate(&mut self) {
        self.active = true;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn set_distance(&mut self, distance: Distance) {
        self.distance = Some(distance);
    }

    pub fn set_vector(&mut self, vector: MeasurementVector) {
        self.vector = Some(vector);
    }

    pub fn set_analysis(&mut self, analysis: Analysis) {
        self.analysis = Some(analysis);
    }
}
