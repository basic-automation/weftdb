use batch_module::*;

mod assets;
mod batch_module;
mod influxdb2;
mod sources;

#[tokio::main]
async fn main() {
	println!("starting...");

	// start timer
	let timer = std::time::Instant::now();

	// create batch module
	let params = BatchModuleParameters::new(BatchLength::ThreeHour, sources::Source::AgrogateBtcBtcAndUsaUsd);
	let mut batch_module = BatchModule::new(params);
	batch_module.process_batches().await;
        batch_module.send().await;

	// stop timer
	let duration = timer.elapsed();
	println!("duration: {:?}", duration);
}
