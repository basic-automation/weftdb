use super::parameters::*;
use crate::assets::Asset;
use crate::influxdb2::*;
use crate::sources::Source;
use uuid::Uuid;

pub struct Measurement {
	pub measurement_timestamp: i64,
	pub measurement_asset_1: Asset,
	pub measurement_asset_2: Asset,
	pub measurement_source: Source,
	pub measurement_uuid: String,
	pub measurement_ratio: f64,
}

impl Measurement {
	pub fn new(timestamp: i64, ratio: f64, parameters: MeasurementParameters) -> Self {
		Self {
			measurement_timestamp: timestamp,
			measurement_asset_1: parameters.measurement_asset_1,
			measurement_asset_2: parameters.measurement_asset_2,
			measurement_source: parameters.measurement_source,
			measurement_uuid: Uuid::new_v4().to_string(),
			measurement_ratio: ratio,
		}
	}

	pub async fn emit(&self) {
		// save to database
		let time = InfluxTimestamp::Unix(self.measurement_timestamp);
		let field = InfluxField::Float(self.measurement_ratio);

		// save source
		let mut influx = Influxdb2::new();
		influx.connection("http://localhost:8086", "ahcKtYeRXQ4xYTivH4x4zSHnH9ZYMhAcr9uIChGMQdNAsHylBDk2-E7Cu1T9eMAwdLRGCWzFJ0nW_f5picHDAw==", "Mim", "dsm_measurements").await;
		influx.new_measurement(&self.measurement_source.to_string()).await;
		influx.add_tag("measurement_asset_1", &self.measurement_asset_1.to_string()).await;
		influx.add_tag("measurement_asset_2", &self.measurement_asset_2.to_string()).await;
		influx.add_tag("measurement_source", &self.measurement_source.to_string()).await;
		influx.add_field("measurement_ratio", field.clone()).await;
		influx.add_timestamp(time.clone()).await;
		influx.write().await;

		// save to source agrogate
		influx.new_measurement(&self.measurement_source.aggrogate_source().await.to_string()).await;
		influx.add_tag("measurement_asset_1", &self.measurement_asset_1.to_string()).await;
		influx.add_tag("measurement_asset_2", &self.measurement_asset_2.to_string()).await;
		influx.add_tag("measurement_source", &self.measurement_source.to_string()).await;
		influx.add_field("measurement_ratio", field).await;
		influx.add_timestamp(time).await;
		influx.write().await;
	}

	pub async fn emit_event(&self) {
		// save to database
		let time = InfluxTimestamp::Unix(chrono::Utc::now().timestamp());
		let field = InfluxField::String(self.measurement_uuid.clone());

		// save source
		let mut influx = Influxdb2::new();
		influx.connection("http://localhost:8086", "ahcKtYeRXQ4xYTivH4x4zSHnH9ZYMhAcr9uIChGMQdNAsHylBDk2-E7Cu1T9eMAwdLRGCWzFJ0nW_f5picHDAw==", "Mim", "dsm_events").await;
		influx.new_measurement("measurement_new").await;
		influx.add_tag("measurement_asset_1", &self.measurement_asset_1.to_string()).await;
		influx.add_tag("measurement_asset_2", &self.measurement_asset_2.to_string()).await;
		influx.add_tag("measurement_source", &self.measurement_source.to_string()).await;
		influx.add_field("measurement_uuid", field.clone()).await;
		influx.add_timestamp(time.clone()).await;
		influx.write().await;

		// save to source agrogate
		influx.new_measurement("measurement_new").await;
		influx.add_tag("measurement_asset_1", &self.measurement_asset_1.to_string()).await;
		influx.add_tag("measurement_asset_2", &self.measurement_asset_2.to_string()).await;
		influx.add_tag("measurement_source", &self.measurement_source.aggrogate_source().await.to_string()).await;
		influx.add_field("measurement_uuid", field).await;
		influx.add_timestamp(time).await;
		influx.write().await;
	}
}
