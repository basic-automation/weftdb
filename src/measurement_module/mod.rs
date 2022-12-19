use crate::sources::*;
pub use interval::*;
pub use measurement::*;
pub use parameters::*;

mod interval;
mod measurement;
mod parameters;

pub struct MeasurementModule {
	measurement_parameters: MeasurementParameters,
}

impl MeasurementModule {
	pub fn new(measurement_parameters: MeasurementParameters) -> Self {
		Self { measurement_parameters }
	}

	pub async fn measure(&self) {
		// check if api is available
		let is_active = match &self.measurement_parameters.measurement_source {
			Source::ThorchainBtcBtcAndUsaUsd(ninerealms) => ninerealms.pool_is_active("BTC.BTC").await,
			_ => false,
		};

		if is_active {
			// annouce active api
			println!("{} is active", self.measurement_parameters.measurement_source);

			// get quote
			let quotes = match &self.measurement_parameters.measurement_source {
				Source::ThorchainBtcBtcAndUsaUsd(ninerealms) => {
					let pool = &self.measurement_parameters.source_asset().await.to_string();
					let depth_interval = self.measurement_parameters.measurement_interval.to_depth_interval();
					let count = self.measurement_parameters.measurement_reach;
					Some(ninerealms.get_usd_quote(pool, depth_interval, count as u16, None, None).await)
				}
				_ => None,
			};

			if let Some(quotes) = quotes {
				for (time, price) in quotes {
					// annouce quote
					println!("{}: time: {}, price: {}", self.measurement_parameters.measurement_source, time, price);

					// create measurement
					let measurement = Measurement::new(time, price, self.measurement_parameters.clone());
					measurement.emit().await;
					measurement.emit_event().await;
				}
			}
		}
	}
}
