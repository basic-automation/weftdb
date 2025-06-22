use database::Measurement;
use bigdecimal::BigDecimal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trend {
    destination: Measurement,
    slope: BigDecimal,
}

impl Trend {
    pub fn new(destination: Measurement, slope: BigDecimal) -> Self {
        Self { destination, slope }
    }

    pub fn destination(&self) -> &Measurement {
        &self.destination
    }

    pub fn slope(&self) -> &BigDecimal {
        &self.slope
    }

    pub fn set_destination(&mut self, destination: Measurement) {
        self.destination = destination;
    }

    pub fn set_slope(&mut self, slope: BigDecimal) {
        self.slope = slope;
    }
}