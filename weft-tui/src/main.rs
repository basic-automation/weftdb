#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

use std::fs::OpenOptions;

use weft_tui::{
	logging::{LogBuffer, LogBufferLayer}, run
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() {
	// Create log file path
	let log_path = std::env::var("WEFT_DATA_DIR").map_or_else(
		|_| {
			let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
			home.join(".weftdb").join("weft-tui.log")
		},
		|data_dir| std::path::PathBuf::from(data_dir).join("weft-tui.log"),
	);

	// Create parent directories if they don't exist
	if let Some(parent) = log_path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}

	// Open log file in append mode
	let file = OpenOptions::new().create(true).append(true).open(&log_path).expect("Failed to create log file");

	// Create log buffer for UI display (stores last 50 messages)
	let log_buffer = LogBuffer::new(50);

	// Initialize tracing with both file and in-app buffer output
	tracing_subscriber::registry().with(fmt::layer().with_writer(std::sync::Arc::new(file)).with_ansi(false).with_target(true).with_level(true).with_thread_ids(true)).with(LogBufferLayer::new(log_buffer.clone())).with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("weft_tui=debug,database=debug,info"))).init();

	eprintln!("📝 Logs written to: {}", log_path.display());
	tracing::info!("=== Starting WeftDB TUI application ===");

	if let Err(e) = run(log_buffer).await {
		tracing::error!("Fatal error: {}", e);
		eprintln!("Error: {e}");
		std::process::exit(1);
	}

	tracing::info!("=== Application shut down gracefully ===");
}
