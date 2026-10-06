//! Background GPU calibration.
//!
//! splimes' default `Backend::Auto` never uses the GPU on its own: only once the program
//! has started it, and only above thresholds that are "never" by default. Calibration
//! ([`splimes::calibrate`]) starts the GPU if there is one, times every backend on this
//! machine, and sets `Auto`'s thresholds to the measured crossovers — the rayon pool as
//! well as the GPU, so it is worth running on a machine without a GPU too.
//!
//! The binary starts [`calibrate`] once, right after it binds its listener, on tokio's
//! blocking pool ([`spawn_in_background`]), so it never delays or blocks serving. It takes
//! several seconds and a few hundred megabytes while it runs (its largest grid is 16 Mi
//! points). splimes installs the measured thresholds only once every measurement is done;
//! until then `Auto` keeps its defaults, which never pick the GPU, so requests in the
//! meantime interpolate on the CPU. Calibration never stops the server: without a usable
//! GPU, or if calibration itself fails or panics, interpolation stays on the CPU backends
//! with splimes' defaults.
//!
//! [`GPU_CALIBRATE_ENV`] selects the [`CalibrationMode`]: `0` skips calibration, and
//! `force` runs it even when the adapter is a CPU/software rasteriser, which it otherwise
//! skips ([`is_software_adapter`]).

use std::time::Instant;

use splimes::AutoThresholds;
use tokio::task::JoinHandle;

/// Environment variable that controls the startup calibration ([`CalibrationMode`]).
///
/// `0` (or `false` / `no` / `off`, case-insensitive) skips it, `force` runs it even on a
/// CPU/software adapter, and unset or anything else runs it unless the adapter is a
/// CPU/software one.
pub const GPU_CALIBRATE_ENV: &str = "WEFT_GPU_CALIBRATE";

/// Whether, and how, the startup calibration runs; parsed from [`GPU_CALIBRATE_ENV`] by
/// [`CalibrationMode::from_env_value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationMode {
	/// Don't calibrate (`0`, `false`, `no`, `off`): `Auto` keeps splimes' defaults.
	Off,
	/// Calibrate, unless `prewarm_gpu()` reports a CPU/software adapter (the default).
	Auto,
	/// Calibrate even on a CPU/software adapter (`force`).
	Force,
}

impl CalibrationMode {
	/// The mode for a value of [`GPU_CALIBRATE_ENV`] (`None` when it is unset): explicitly
	/// falsey is [`Off`](Self::Off), `force` is [`Force`](Self::Force), and anything else
	/// is [`Auto`](Self::Auto). Case and surrounding whitespace are ignored.
	#[must_use]
	pub fn from_env_value(value: Option<&str>) -> Self {
		match value.map(|raw| raw.trim().to_ascii_lowercase()).as_deref() {
			Some("0" | "false" | "no" | "off") => Self::Off,
			Some("force") => Self::Force,
			_ => Self::Auto,
		}
	}
}

/// Whether a GPU adapter is really the CPU.
///
/// That is a software rasteriser such as Mesa's llvmpipe/lavapipe or Windows' WARP
/// ("Microsoft Basic Render Driver"). splimes reports these with the device type `"cpu"`;
/// the names are matched too, for drivers that report another type.
///
/// Calibrating one only times shaders running on the cores that also serve requests, so
/// [`calibrate`] skips it unless the mode is [`CalibrationMode::Force`].
#[must_use]
pub fn is_software_adapter(device_type: &str, name: &str) -> bool {
	let name = name.to_ascii_lowercase();
	device_type.eq_ignore_ascii_case("cpu") || ["llvmpipe", "lavapipe", "softpipe", "swiftshader", "microsoft basic render driver"].iter().any(|software| name.contains(software))
}

/// Start the GPU and calibrate `Backend::Auto`, returning log lines that describe it.
///
/// The first line names the adapter in use, or why there is none; the second gives the
/// thresholds now in force, why calibration failed and left the defaults, or why it was
/// skipped (a CPU/software adapter, unless `mode` is [`CalibrationMode::Force`]).
/// Blocking, and takes several seconds: call it off the async runtime, once, and only
/// with a `mode` other than [`CalibrationMode::Off`].
#[must_use]
pub fn calibrate(mode: CalibrationMode) -> Vec<String> {
	let started = Instant::now();
	let gpu = match splimes::prewarm_gpu() {
		Ok(info) => {
			let gpu = format!("gpu: {} ({}, {}; f64 shaders: {})", info.name, info.api, info.device_type, if info.supports_f64 { "yes" } else { "no" });
			if mode != CalibrationMode::Force && is_software_adapter(&info.device_type, &info.name) {
				let skipped = format!("gpu calibration: skipped, because {} is a CPU/software adapter: its shaders run on the same cores that serve requests, so timing it would only load them. Interpolation stays on the CPU with the default thresholds ({}); set {GPU_CALIBRATE_ENV}=force to calibrate anyway", info.name, describe_thresholds(splimes::auto_thresholds()));
				return vec![gpu, skipped];
			}
			gpu
		}
		Err(err) => format!("gpu: unavailable ({err}); interpolation stays on the CPU"),
	};
	let thresholds = match splimes::calibrate() {
		Ok(calibration) => format!("gpu calibration: done in {:.1} s: {}", started.elapsed().as_secs_f64(), describe_thresholds(calibration.thresholds)),
		Err(err) => format!("gpu calibration: failed ({err}); keeping the default thresholds ({})", describe_thresholds(splimes::auto_thresholds())),
	};
	vec![gpu, thresholds]
}

/// Run `work` (normally `move || calibrate(mode)`) in the background.
///
/// `work` runs on tokio's blocking pool, and the lines it returns are printed when it
/// finishes; nothing waits for it, so the caller goes on serving while it runs. A panic in
/// `work` is reported, not propagated, and leaves splimes' defaults in force.
///
/// Must be called from within a tokio runtime.
pub fn spawn_in_background<F>(work: F) -> JoinHandle<()>
where
	F: FnOnce() -> Vec<String> + Send + 'static,
{
	let task = tokio::task::spawn_blocking(work);
	tokio::spawn(async move {
		match task.await {
			Ok(lines) => lines.iter().for_each(|line| println!("{line}")),
			Err(err) => eprintln!("gpu calibration: aborted ({err}); interpolation stays on the CPU with the default thresholds"),
		}
	})
}

/// `Auto`'s thresholds in words, e.g. `rayon from 65536 grid points, GPU from never (f64) / 262144 (f32)`.
fn describe_thresholds(thresholds: AutoThresholds) -> String {
	let points = |n: usize| if n == usize::MAX { "never".to_string() } else { n.to_string() };
	format!("rayon from {} grid points, GPU from {} (f64) / {} (f32)", points(thresholds.parallel_min_points), points(thresholds.gpu_min_points), points(thresholds.gpu_f32_min_points))
}

#[cfg(test)]
mod tests {
	use std::sync::mpsc;

	use axum::{body::Body, http::Request};
	use tower::ServiceExt as _;

	use super::*;

	#[test]
	fn calibration_is_opt_out_and_can_be_forced() {
		assert_eq!(CalibrationMode::from_env_value(None), CalibrationMode::Auto, "on by default");
		for on in ["1", "true", "yes", "", "anything"] {
			assert_eq!(CalibrationMode::from_env_value(Some(on)), CalibrationMode::Auto, "{on:?} keeps it on");
		}
		for off in ["0", "false", "FALSE", " no ", "off"] {
			assert_eq!(CalibrationMode::from_env_value(Some(off)), CalibrationMode::Off, "{off:?} turns it off");
		}
		for force in ["force", "FORCE", " Force "] {
			assert_eq!(CalibrationMode::from_env_value(Some(force)), CalibrationMode::Force, "{force:?} forces it");
		}
	}

	#[test]
	fn software_adapters_are_recognised() {
		// The device type splimes reports for a software rasteriser, on Vulkan and DX12.
		assert!(is_software_adapter("cpu", "llvmpipe (LLVM 19.1.7, 256 bits)"));
		assert!(is_software_adapter("cpu", "Microsoft Basic Render Driver"));
		// The names, for a driver that reports another type.
		assert!(is_software_adapter("other", "llvmpipe (LLVM 19.1.7, 256 bits)"));
		assert!(is_software_adapter("other", "Lavapipe"));
		assert!(is_software_adapter("virtual", "SwiftShader Device (Subzero)"));
		assert!(is_software_adapter("other", "Microsoft Basic Render Driver"));
		// Real GPUs.
		assert!(!is_software_adapter("discrete", "NVIDIA GeForce RTX 4070 Ti SUPER"));
		assert!(!is_software_adapter("integrated", "Apple M2"));
		assert!(!is_software_adapter("integrated", "AMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)"));
	}

	#[test]
	fn thresholds_read_never_for_an_unused_backend() {
		assert_eq!(describe_thresholds(AutoThresholds::DEFAULT), "rayon from 65536 grid points, GPU from never (f64) / never (f32)");
		assert_eq!(describe_thresholds(AutoThresholds::new(4096, 1 << 20).with_gpu_f32(1 << 18)), "rayon from 4096 grid points, GPU from 1048576 (f64) / 262144 (f32)");
	}

	/// The calibration runs beside serving: while it is still working, the router answers,
	/// and it finishes on its own afterwards.
	#[tokio::test]
	async fn calibration_runs_in_the_background_while_requests_are_served() {
		let (finish, finished) = mpsc::channel::<()>();
		let calibration = spawn_in_background(move || {
			finished.recv().expect("the test releases the calibration");
			vec!["gpu calibration: done".to_string()]
		});

		let response = crate::app().oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), 200, "served while the calibration is still running");
		assert!(!calibration.is_finished(), "the calibration is still blocked");

		finish.send(()).unwrap();
		calibration.await.expect("the background task completes");
	}

	/// A panicking calibration is reported by the background task, which still completes.
	#[tokio::test]
	async fn a_panicking_calibration_does_not_take_the_server_down() {
		let calibration = spawn_in_background(|| panic!("calibration exploded"));
		calibration.await.expect("the panic stays inside the blocking task");
		let response = crate::app().oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap()).await.unwrap();
		assert_eq!(response.status(), 200);
	}
}
