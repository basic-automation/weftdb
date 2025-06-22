use crate::types::BatchedMeasurement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    batch_size: usize,
    measurements: Vec<BatchedMeasurement>,
}

impl Batch {
    pub fn new(batch_size: usize, measurements: Vec<BatchedMeasurement>) -> Self {
        Self {
            batch_size,
            measurements,
        }
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn measurements(&self) -> &Vec<BatchedMeasurement> {
        &self.measurements
    }

    pub fn set_measurements(&mut self, measurements: Vec<BatchedMeasurement>) {
        self.measurements = measurements;
    }

    
}