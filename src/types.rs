use database::Measurement;
use bigdecimal::BigDecimal;

pub struct BatchedMeasurement {
    active: bool,
    measurement: Measurement,
    distance: Option<Distance>,
    vector: Option<MeasurementVector>,
    analysis: Option<Analysis>,
}

pub struct Distance {
    positive: BigDecimal,
    negative: BigDecimal,
}

pub struct MeasurementVector {
    location: BigDecimal,
    amplitude: BigDecimal,
}

pub struct Trend {
    destination: Measurement,
    slope: BigDecimal,
}

pub struct Relative {
    vector: MeasurementVector,
    max_x: BigDecimal,
    max_y: BigDecimal,
}

pub struct Analysis {
    trend: Option<Vec<Trend>>,
    relative: Option<Relative>,
}

pub struct Batch {
    batch_size: usize,
    measurements: Vec<BatchedMeasurement>,
}