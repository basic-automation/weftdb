use assets::*;
use measurement_module::*;
use sources::*;
use tokio::time::{sleep, Duration};

mod assets;
mod influxdb2;
mod measurement_module;
mod sources;

#[tokio::main]
async fn main() {
	loop {
		let measurement_parameters = MeasurementParameters::new(Source::ThorchainBtcBtcAndUsaUsd(ThorchainNinerealms::new()), Asset::BtcBtc, Asset::UsaUsd, Interval::FiveMinute, 5);
		let m_m = MeasurementModule::new(measurement_parameters);
		m_m.measure().await;
		sleep(Duration::from_secs(300)).await;
	}
}
