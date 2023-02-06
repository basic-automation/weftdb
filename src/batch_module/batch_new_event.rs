use crate::assets::Asset;
use crate::influxdb2::FluxInterpolation;
use crate::sources::Source;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BatchNewEvent {
	pub batch_start: i64,
	pub batch_end: i64,
	pub batch_size: usize,
	pub batch_source: Source,
	pub batch_asset_1: Asset,
	pub batch_asset_2: Asset,
	pub batch_interval: u64,
	pub batch_interpolation: FluxInterpolation,
	pub batch_measurement_event_uuid: String,
}

impl BatchNewEvent {
	pub fn name() -> String {
		"batch_new".to_string()
	}
}
