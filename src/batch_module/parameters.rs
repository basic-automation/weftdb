#![allow(dead_code)]
use super::length::ToSeconds as BatchLengthToSeconds;
use super::*;
use crate::assets::*;
use crate::influxdb2::{FluxInterpolation, ToFluxInterpolation, ToSeconds};
use crate::sources::Source;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BatchModuleParameters {
	pub batch_length: BatchLength,
	pub batch_source: Source,
}

impl BatchModuleParameters {
	pub fn new(batch_length: BatchLength, batch_source: Source) -> BatchModuleParameters {
		BatchModuleParameters { batch_length, batch_source }
	}

	pub async fn assets(&self) -> (Asset, Asset) {
		self.batch_source.to_assets()
	}

	pub async fn size(&self) -> usize {
		self.batch_length.to_seconds() as usize
	}

	pub async fn interpolation(&self) -> FluxInterpolation {
		self.batch_length.to_interpolation()
	}

	pub async fn interval(&self) -> u64 {
		let size = self.size().await;
		let unit = self.interpolation().await.to_seconds();

		if unit == 0 {
			0
		} else {
			size as u64 / unit
		}
	}
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeasurementParameters {
	pub measurement_parameter_asset_1: Asset,
	pub measurement_parameter_asset_2: Asset,
	pub measurement_parameter_size: usize,
	pub measurement_parameter_interval: u64,
	pub measurement_parameter_interpolation: FluxInterpolation,
	pub measurement_parameter_used_measurement_event_uuids: Vec<String>,
	pub measurement_parameter_source: Source,
}

impl MeasurementParameters {
	pub async fn new(batch_parameters: BatchModuleParameters, measurement_parameter_used_measurement_event_uuids: Vec<String>) -> MeasurementParameters {
		let (measurement_parameter_asset_1, measurement_parameter_asset_2) = batch_parameters.assets().await;
		let measurement_parameter_size = batch_parameters.size().await;
		let measurement_parameter_interval = batch_parameters.interval().await;
		let measurement_parameter_interpolation = batch_parameters.interpolation().await;
		let measurement_parameter_source = batch_parameters.batch_source;

		MeasurementParameters {
			measurement_parameter_asset_1,
			measurement_parameter_asset_2,
			measurement_parameter_size,
			measurement_parameter_interval,
			measurement_parameter_interpolation,
			measurement_parameter_used_measurement_event_uuids,
			measurement_parameter_source,
		}
	}
}
