//! Startup GPU calibration.
//!
//! splimes' default `Backend::Auto` never uses the GPU on its own: only once the program
//! has started it, and only above thresholds that are "never" by default. Calibration
//! ([`splimes::calibrate`]) starts the GPU if there is one, times every backend on this
//! machine, and sets `Auto`'s thresholds to the measured crossovers — the rayon pool as
//! well as the GPU, so it is worth running on a machine without a GPU too.
//!
//! The binary runs [`calibrate`] once at startup, on a blocking thread, unless
//! `WEFT_GPU_CALIBRATE=0` ([`calibration_enabled`]). It takes several seconds and a few
//! hundred megabytes while it runs (its largest grid is 16 Mi points), and it never fails
//! startup: without a usable GPU, or if calibration itself fails, interpolation stays on
//! the CPU backends with splimes' defaults.

use splimes::AutoThresholds;

/// Environment variable that opts out of the startup calibration when set to `0` (or
/// `false` / `no` / `off`, case-insensitive). Unset or anything else runs it.
pub const GPU_CALIBRATE_ENV: &str = "WEFT_GPU_CALIBRATE";

/// Whether the startup calibration runs, given the value of [`GPU_CALIBRATE_ENV`]: on,
/// unless the variable is explicitly falsey.
#[must_use]
pub fn calibration_enabled(value: Option<&str>) -> bool {
	!value.is_some_and(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
}

/// Start the GPU and calibrate `Backend::Auto`, returning log lines that describe it.
///
/// The first line names the adapter in use, or why there is none; the second gives the
/// thresholds now in force, or why calibration failed and left the defaults. Blocking,
/// and takes several seconds: call it off the async runtime, once.
#[must_use]
pub fn calibrate() -> Vec<String> {
	let gpu = match splimes::prewarm_gpu() {
		Ok(info) => format!("gpu: {} ({}, {}; f64 shaders: {})", info.name, info.api, info.device_type, if info.supports_f64 { "yes" } else { "no" }),
		Err(err) => format!("gpu: unavailable ({err}); interpolation stays on the CPU"),
	};
	let thresholds = match splimes::calibrate() {
		Ok(calibration) => format!("gpu calibration: {}", describe_thresholds(calibration.thresholds)),
		Err(err) => format!("gpu calibration: failed ({err}); keeping the default thresholds ({})", describe_thresholds(splimes::auto_thresholds())),
	};
	vec![gpu, thresholds]
}

/// `Auto`'s thresholds in words, e.g. `rayon from 65536 grid points, GPU from never (f64) / 262144 (f32)`.
fn describe_thresholds(thresholds: AutoThresholds) -> String {
	let points = |n: usize| if n == usize::MAX { "never".to_string() } else { n.to_string() };
	format!("rayon from {} grid points, GPU from {} (f64) / {} (f32)", points(thresholds.parallel_min_points), points(thresholds.gpu_min_points), points(thresholds.gpu_f32_min_points))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn calibration_is_opt_out() {
		assert!(calibration_enabled(None), "on by default");
		for on in ["1", "true", "yes", "", "anything"] {
			assert!(calibration_enabled(Some(on)), "{on:?} keeps it on");
		}
		for off in ["0", "false", "FALSE", " no ", "off"] {
			assert!(!calibration_enabled(Some(off)), "{off:?} turns it off");
		}
	}

	#[test]
	fn thresholds_read_never_for_an_unused_backend() {
		assert_eq!(describe_thresholds(AutoThresholds::DEFAULT), "rayon from 65536 grid points, GPU from never (f64) / never (f32)");
		assert_eq!(describe_thresholds(AutoThresholds::new(4096, 1 << 20).with_gpu_f32(1 << 18)), "rayon from 4096 grid points, GPU from 1048576 (f64) / 262144 (f32)");
	}
}
