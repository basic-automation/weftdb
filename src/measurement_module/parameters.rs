use super::interval::*;
use crate::assets::Asset;
use crate::assets::*;
use crate::sources::Source;

/// MeasurementParameters is a struct that holds all the parameters for a measurement.
/// Source: The api source of the measurment
/// Asset_1: The first asset of the measurement
/// Asset_2: The second asset of the measurement
/// Interval: The amount of time between each measurement
/// Reach: The number of measurements to be taken. The first measurement is current and others will be furthur in the past.
/// (eg. if interval is 1 minute and reach is 10, the first measurement will be current and the last will be 10 minutes ago)

#[derive(Debug, Clone)]
pub struct MeasurementParameters {
	pub measurement_source: Source,
	pub measurement_asset_1: Asset,
	pub measurement_asset_2: Asset,
	pub measurement_interval: Interval,
	pub measurement_reach: usize,
}

impl MeasurementParameters {
	pub fn new(source: Source, asset_1: Asset, asset_2: Asset, interval: Interval, reach: usize) -> Self {
		Self { measurement_source: source, measurement_asset_1: asset_1, measurement_asset_2: asset_2, measurement_interval: interval, measurement_reach: reach }
	}

	pub async fn source_asset(&self) -> Asset {
		self.measurement_source.to_asset()
	}
}
