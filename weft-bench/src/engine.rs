//! The interpolation engine's backend selection for a run.
//!
//! splimes' default `Backend::Auto` picks one CPU thread, the rayon pool or the GPU by
//! output-grid size, using thresholds that are splimes' defaults (rayon from 64 Ki points,
//! the GPU never) until something measures this machine. `weft-server` calibrates at
//! startup ([`splimes::calibrate`]), so the benchmark does the same before an
//! interpolation run — otherwise Weft-Bench would time a CPU-only engine that the server
//! never runs. [`calibrate`] mirrors the server: it starts the GPU, skips the
//! measurement on a CPU/software adapter (llvmpipe, lavapipe, WARP), and records what it
//! found as an [`EngineMetadata`] in the report's run metadata, beside the hardware.
//!
//! `--no-gpu-calibrate` skips all of it ([`EngineMetadata::uncalibrated`]); the report
//! then records splimes' default thresholds, which never use the GPU.

use serde::{Deserialize, Serialize};
use splimes::AutoThresholds;

/// How `Backend::Auto`'s thresholds were set for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationStatus {
	/// `splimes::calibrate()` measured this machine and set the thresholds.
	Calibrated,
	/// Not calibrated, by request (`--no-gpu-calibrate`): splimes' defaults.
	Disabled,
	/// Not calibrated because the GPU is a CPU/software adapter, as `weft-server` does
	/// by default: splimes' defaults.
	SoftwareAdapter,
	/// Calibration ran and failed: splimes' defaults.
	Failed,
}

/// The interpolation backends in force for a run: whether they were calibrated, the GPU
/// in use, and the grid sizes from which `Backend::Auto` switches backend.
///
/// The thresholds are `None` where `Auto` never uses that backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMetadata {
	/// How the thresholds were set.
	pub calibration: CalibrationStatus,
	/// The GPU adapter splimes started, e.g. `NVIDIA GeForce RTX 4070 Ti SUPER (vulkan,
	/// discrete; f64 shaders: yes)`; `None` when calibration was disabled or there is no
	/// usable GPU.
	pub gpu: Option<String>,
	/// The GPU driver's name and version as the driver reports them, e.g. `NVIDIA
	/// 580.82.09`; `None` with no GPU, or when the driver reports nothing.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub gpu_driver: Option<String>,
	/// Why there is no GPU, or why calibration was skipped or failed, when it was.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub note: Option<String>,
	/// `Auto` uses the rayon pool from this many output grid points.
	pub parallel_min_points: Option<usize>,
	/// `Auto` uses the GPU, in `f64`, from this many output grid points.
	pub gpu_min_points: Option<usize>,
	/// `Auto` uses the GPU, in `f32` (only when a caller asks for that precision), from
	/// this many output grid points.
	pub gpu_f32_min_points: Option<usize>,
}

impl EngineMetadata {
	/// Metadata for `status`, the GPU description and note, and `thresholds`.
	#[must_use]
	pub fn new(calibration: CalibrationStatus, gpu: Option<String>, note: Option<String>, thresholds: AutoThresholds) -> Self {
		let threshold = |n: usize| (n != usize::MAX).then_some(n);
		Self { calibration, gpu, gpu_driver: None, note, parallel_min_points: threshold(thresholds.parallel_min_points), gpu_min_points: threshold(thresholds.gpu_min_points), gpu_f32_min_points: threshold(thresholds.gpu_f32_min_points) }
	}

	/// The same metadata with the GPU driver recorded; an empty or blank `driver` (some
	/// drivers report none) records nothing.
	#[must_use]
	pub fn with_gpu_driver(mut self, driver: &str) -> Self {
		let driver = driver.trim();
		self.gpu_driver = (!driver.is_empty()).then(|| driver.to_string());
		self
	}

	/// The GPU and its driver in words, e.g. `NVIDIA GeForce RTX 4070 Ti SUPER (vulkan,
	/// discrete; f64 shaders: yes), driver NVIDIA 580.82.09`; `None` with no GPU.
	#[must_use]
	pub fn describe_gpu(&self) -> Option<String> {
		let gpu = self.gpu.as_deref()?;
		Some(self.gpu_driver.as_deref().map_or_else(|| gpu.to_string(), |driver| format!("{gpu}, driver {driver}")))
	}

	/// The engine as it stands without calibration (`--no-gpu-calibrate`): splimes'
	/// current thresholds, no GPU started.
	#[must_use]
	pub fn uncalibrated() -> Self {
		Self::new(CalibrationStatus::Disabled, None, Some("--no-gpu-calibrate".to_string()), splimes::auto_thresholds())
	}

	/// The thresholds in words, e.g. `rayon from 16384 grid points, GPU from 4096 (f64) /
	/// never (f32)`.
	#[must_use]
	pub fn describe_thresholds(&self) -> String {
		let points = |n: Option<usize>| n.map_or_else(|| "never".to_string(), |n| n.to_string());
		format!("rayon from {} grid points, GPU from {} (f64) / {} (f32)", points(self.parallel_min_points), points(self.gpu_min_points), points(self.gpu_f32_min_points))
	}

	/// The calibration status in words, with its note when there is one.
	#[must_use]
	pub fn describe_calibration(&self) -> String {
		let status = match self.calibration {
			CalibrationStatus::Calibrated => "calibrated",
			CalibrationStatus::Disabled => "not calibrated",
			CalibrationStatus::SoftwareAdapter => "not calibrated (software adapter)",
			CalibrationStatus::Failed => "calibration failed",
		};
		match &self.note {
			Some(note) if self.calibration != CalibrationStatus::SoftwareAdapter => format!("{status}: {note}"),
			_ => status.to_string(),
		}
	}
}

/// Whether a GPU adapter is really the CPU: a software rasteriser such as Mesa's
/// llvmpipe/lavapipe or Windows' WARP ("Microsoft Basic Render Driver").
///
/// The same rule as `weft_server::gpu::is_software_adapter` (Weft-Bench drives the stack
/// from outside and does not link the server): splimes reports these with the device
/// type `"cpu"`, and the names are matched too, for drivers that report another type.
#[must_use]
pub fn is_software_adapter(device_type: &str, name: &str) -> bool {
	let name = name.to_ascii_lowercase();
	device_type.eq_ignore_ascii_case("cpu") || ["llvmpipe", "lavapipe", "softpipe", "swiftshader", "microsoft basic render driver"].iter().any(|software| name.contains(software))
}

/// Start the GPU and calibrate `Backend::Auto` as `weft-server` does at startup, and
/// describe the result.
///
/// Blocking, and takes several seconds (splimes times every backend on grids up to 16 Mi
/// points): call it once, before the timed runs and off the async runtime. Never fails:
/// without a usable GPU, on a software adapter, or if calibration fails, the defaults stay
/// in force and the metadata says why.
#[must_use]
pub fn calibrate() -> EngineMetadata {
	let (gpu, driver, unavailable) = match splimes::prewarm_gpu() {
		Ok(info) => {
			let gpu = format!("{} ({}, {}; f64 shaders: {})", info.name, info.api, info.device_type, if info.supports_f64 { "yes" } else { "no" });
			if is_software_adapter(&info.device_type, &info.name) {
				let note = format!("{} is a CPU/software adapter, which weft-server does not calibrate either", info.name);
				return EngineMetadata::new(CalibrationStatus::SoftwareAdapter, Some(gpu), Some(note), splimes::auto_thresholds()).with_gpu_driver(&info.driver);
			}
			(Some(gpu), info.driver, None)
		}
		Err(err) => (None, String::new(), Some(err.to_string())),
	};
	let engine = match splimes::calibrate() {
		Ok(calibration) => EngineMetadata::new(CalibrationStatus::Calibrated, gpu, unavailable, calibration.thresholds),
		Err(err) => EngineMetadata::new(CalibrationStatus::Failed, gpu, Some(err.to_string()), splimes::auto_thresholds()),
	};
	engine.with_gpu_driver(&driver)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn thresholds_record_never_as_none() {
		let engine = EngineMetadata::new(CalibrationStatus::Disabled, None, None, AutoThresholds::DEFAULT);
		assert_eq!((engine.parallel_min_points, engine.gpu_min_points, engine.gpu_f32_min_points), (Some(65_536), None, None));
		assert_eq!(engine.describe_thresholds(), "rayon from 65536 grid points, GPU from never (f64) / never (f32)");
		let calibrated = EngineMetadata::new(CalibrationStatus::Calibrated, Some("Test GPU (vulkan, discrete; f64 shaders: yes)".to_string()), None, AutoThresholds::new(16_384, 4_096).with_gpu_f32(1 << 18));
		assert_eq!(calibrated.describe_thresholds(), "rayon from 16384 grid points, GPU from 4096 (f64) / 262144 (f32)");
		assert_eq!(calibrated.describe_calibration(), "calibrated");
	}

	#[test]
	fn uncalibrated_engine_records_the_flag_and_starts_no_gpu() {
		let engine = EngineMetadata::uncalibrated();
		assert_eq!(engine.calibration, CalibrationStatus::Disabled);
		assert_eq!(engine.gpu, None);
		assert_eq!(engine.describe_calibration(), "not calibrated: --no-gpu-calibrate");
	}

	#[test]
	fn engine_metadata_round_trips_through_json() {
		let engine = EngineMetadata::new(CalibrationStatus::SoftwareAdapter, Some("llvmpipe (vulkan, cpu; f64 shaders: yes)".to_string()), Some("llvmpipe is a CPU/software adapter".to_string()), AutoThresholds::DEFAULT);
		let json = serde_json::to_value(&engine).unwrap();
		assert_eq!(json["calibration"], "software_adapter");
		assert_eq!(json["gpu_min_points"], serde_json::Value::Null, "never is null, not usize::MAX");
		assert_eq!(serde_json::from_value::<EngineMetadata>(json).unwrap(), engine);
	}

	#[test]
	fn gpu_driver_is_recorded_beside_the_adapter_and_blank_drivers_are_not() {
		let gpu = "Test GPU (vulkan, discrete; f64 shaders: yes)";
		let engine = EngineMetadata::new(CalibrationStatus::Calibrated, Some(gpu.to_string()), None, AutoThresholds::DEFAULT).with_gpu_driver(" NVIDIA 580.82.09 ");
		assert_eq!(engine.gpu_driver.as_deref(), Some("NVIDIA 580.82.09"));
		assert_eq!(engine.describe_gpu().as_deref(), Some("Test GPU (vulkan, discrete; f64 shaders: yes), driver NVIDIA 580.82.09"));
		let json = serde_json::to_value(&engine).unwrap();
		assert_eq!(json["gpu_driver"], "NVIDIA 580.82.09");
		assert_eq!(serde_json::from_value::<EngineMetadata>(json).unwrap(), engine);

		let blank = EngineMetadata::new(CalibrationStatus::Calibrated, Some(gpu.to_string()), None, AutoThresholds::DEFAULT).with_gpu_driver("  ");
		assert_eq!(blank.gpu_driver, None);
		assert_eq!(blank.describe_gpu().as_deref(), Some(gpu));
		assert!(serde_json::to_value(&blank).unwrap().get("gpu_driver").is_none(), "an absent driver is omitted, not null");
		assert_eq!(EngineMetadata::uncalibrated().describe_gpu(), None);
	}

	#[test]
	fn engine_metadata_without_a_driver_field_still_deserializes() {
		// A schema v16..v19 artifact has no `gpu_driver`.
		let old = serde_json::json!({ "calibration": "calibrated", "gpu": "Test GPU", "parallel_min_points": 16384, "gpu_min_points": 4096, "gpu_f32_min_points": null });
		let engine: EngineMetadata = serde_json::from_value(old).unwrap();
		assert_eq!((engine.gpu.as_deref(), engine.gpu_driver), (Some("Test GPU"), None));
	}

	#[test]
	fn software_adapters_are_recognised_like_the_server_does() {
		assert!(is_software_adapter("cpu", "llvmpipe (LLVM 19.1.7, 256 bits)"));
		assert!(is_software_adapter("cpu", "Microsoft Basic Render Driver"));
		assert!(is_software_adapter("other", "Lavapipe"));
		assert!(is_software_adapter("virtual", "SwiftShader Device (Subzero)"));
		assert!(!is_software_adapter("discrete", "NVIDIA GeForce RTX 4070 Ti SUPER"));
		assert!(!is_software_adapter("integrated", "Apple M2"));
	}
}
