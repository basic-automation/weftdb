use dsm_asset::*;
use dsm_measurement::*;
use dsm_source::*;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
	loop {
		let measurement_parameters = MeasurementParameters::new(Source::ThorchainBtcBtcAndUsaUsd(ThorchainNinerealms::new()), Asset::BtcBtc, Asset::UsaUsd, Interval::FiveMinute, 5);
		let m_m = MeasurementModule::new(measurement_parameters);
		m_m.measure().await;
		sleep(Duration::from_secs(300)).await;
	}
}
