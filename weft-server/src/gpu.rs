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
//!
//! ## Adapter selection and multi-GPU (Phase 5, 5.7)
//!
//! [`GPU_ADAPTER_ENV`] picks the adapter splimes opens (by index from
//! `splimes::gpu_adapters()`, by `api:<name>`, or by a name substring), and
//! [`GPU_DEVICES_ENV`] opens more than one device (`all`, or a comma-separated list of
//! selectors). Both are read once at startup ([`gpu_config_from_env`]) and applied with
//! `splimes::configure_gpu` ([`apply_config`]) before calibration or any request touches
//! the GPU. Unset, nothing is configured and splimes' single-device default is unchanged.
//! Multi-device is opt-in: splimes' own measurement (its BENCHMARKS.md, "More than one
//! GPU") found it slower than one device on a discrete-plus-integrated machine.

use std::time::Instant;

use splimes::{AdapterSelector, AutoThresholds, GpuConfig, GpuDevices};
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

/// Environment variable naming the one adapter splimes opens (5.7): a whole number is an
/// index into `splimes::gpu_adapters()`, `api:<api>` the first adapter on that graphics
/// API, `name:<text>` (or any other text) the first adapter whose name contains it,
/// ignoring case. Unset: splimes' own choice (`WGPU_ADAPTER_NAME`, else the
/// high-performance adapter).
pub const GPU_ADAPTER_ENV: &str = "WEFT_GPU_ADAPTER";

/// Environment variable choosing how many devices splimes opens (5.7): `one` (the
/// default), `all` (every hardware adapter), or a comma-separated list of adapter
/// selectors in [`GPU_ADAPTER_ENV`]'s syntax, opened in that order.
pub const GPU_DEVICES_ENV: &str = "WEFT_GPU_DEVICES";

/// Parse one adapter selector ([`GPU_ADAPTER_ENV`]'s syntax).
///
/// # Errors
///
/// An empty selector, or an empty `api:` / `name:` value.
pub fn parse_adapter_selector(raw: &str) -> Result<AdapterSelector, String> {
	let raw = raw.trim();
	if raw.is_empty() {
		return Err("an adapter selector is empty".to_string());
	}
	if let Ok(index) = raw.parse::<usize>() {
		return Ok(AdapterSelector::Index(index));
	}
	let lower = raw.to_ascii_lowercase();
	let (kind, value) = if lower.starts_with("api:") {
		("api", raw[4..].trim())
	} else if lower.starts_with("name:") {
		("name", raw[5..].trim())
	} else {
		("name", raw)
	};
	if value.is_empty() {
		return Err(format!("the adapter selector {raw:?} names no {kind}"));
	}
	Ok(if kind == "api" { AdapterSelector::Api(value.to_string()) } else { AdapterSelector::Name(value.to_string()) })
}

/// Parse [`GPU_DEVICES_ENV`]: `one`, `all` (any case), or a comma-separated list of
/// adapter selectors.
///
/// # Errors
///
/// An empty value, or a list with an empty or malformed selector.
pub fn parse_devices(raw: &str) -> Result<GpuDevices, String> {
	let trimmed = raw.trim();
	match trimmed.to_ascii_lowercase().as_str() {
		"" => Err("the device list is empty".to_string()),
		"one" => Ok(GpuDevices::One),
		"all" => Ok(GpuDevices::All),
		_ => trimmed.split(',').map(parse_adapter_selector).collect::<Result<Vec<_>, _>>().map(GpuDevices::List),
	}
}

/// The GPU configuration [`GPU_ADAPTER_ENV`] and [`GPU_DEVICES_ENV`] ask for (`None` for
/// a variable that is unset or blank). `Ok(None)` when neither asks for anything, so the
/// server leaves splimes' default (one device, splimes' own adapter choice) untouched.
///
/// # Errors
///
/// A value that does not parse, or an adapter together with `all` or a list: splimes reads
/// the adapter only for a single device, and silently ignoring it would hide the mistake.
pub fn gpu_config_from_env(adapter: Option<&str>, devices: Option<&str>) -> Result<Option<GpuConfig>, String> {
	let adapter = adapter.filter(|raw| !raw.trim().is_empty());
	let devices = devices.filter(|raw| !raw.trim().is_empty());
	if adapter.is_none() && devices.is_none() {
		return Ok(None);
	}
	let devices = devices.map(parse_devices).transpose().map_err(|error| format!("{GPU_DEVICES_ENV}: {error}"))?.unwrap_or_default();
	let mut config = GpuConfig::default();
	if let Some(adapter) = adapter {
		if devices != GpuDevices::One {
			return Err(format!("{GPU_ADAPTER_ENV} picks the adapter for a single device, but {GPU_DEVICES_ENV} opens several; name the adapters in {GPU_DEVICES_ENV} instead"));
		}
		config = config.with_adapter(parse_adapter_selector(adapter).map_err(|error| format!("{GPU_ADAPTER_ENV}: {error}"))?);
	}
	Ok(Some(config.with_devices(devices)))
}

/// Install `config` with `splimes::configure_gpu`, returning the log line. Call it once at
/// startup, before calibration or any request starts the GPU. A refusal (the GPU already
/// started) is reported, not fatal: interpolation still works on splimes' defaults.
#[must_use]
pub fn apply_config(config: GpuConfig) -> String {
	let described = describe_config(&config);
	match splimes::configure_gpu(config) {
		Ok(()) => format!("gpu: configured {described}"),
		Err(err) => format!("gpu: configuration {described} not applied ({err}); splimes' defaults stay"),
	}
}

/// A [`GpuConfig`]'s device choice in words, e.g. `devices: all` or `devices: one (adapter name "4070")`.
fn describe_config(config: &GpuConfig) -> String {
	let selector = |selector: &AdapterSelector| match selector {
		AdapterSelector::Index(index) => format!("#{index}"),
		AdapterSelector::Name(name) => format!("name {name:?}"),
		AdapterSelector::Api(api) => format!("api {api:?}"),
		other => format!("{other:?}"),
	};
	let devices = match &config.devices {
		GpuDevices::One => match &config.adapter {
			Some(adapter) => format!("one (adapter {})", selector(adapter)),
			None => "one".to_string(),
		},
		GpuDevices::All => "all".to_string(),
		GpuDevices::List(list) => format!("list [{}]", list.iter().map(selector).collect::<Vec<_>>().join(", ")),
		other => format!("{other:?}"),
	};
	format!("devices: {devices}")
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
	let mut lines = vec![gpu, thresholds];
	let devices = splimes::gpu_devices();
	if devices.len() > 1 {
		lines.push(format!("gpu devices: {}", devices.iter().enumerate().map(|(index, device)| format!("#{index} {} ({}, {})", device.info.name, device.info.api, device.info.device_type)).collect::<Vec<_>>().join("; ")));
	}
	lines
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

	/// 5.7: unset or blank, nothing is configured, so splimes' single-device default holds.
	#[test]
	fn no_gpu_settings_leave_the_default_alone() {
		assert_eq!(gpu_config_from_env(None, None), Ok(None));
		assert_eq!(gpu_config_from_env(Some(""), Some("  ")), Ok(None));
		assert_eq!(GpuConfig::default().devices, GpuDevices::One, "splimes' default is one device");
	}

	#[test]
	fn adapter_selectors_parse() {
		assert_eq!(parse_adapter_selector("0"), Ok(AdapterSelector::Index(0)));
		assert_eq!(parse_adapter_selector(" 2 "), Ok(AdapterSelector::Index(2)));
		assert_eq!(parse_adapter_selector("api:vulkan"), Ok(AdapterSelector::Api("vulkan".to_string())));
		assert_eq!(parse_adapter_selector("API: Metal"), Ok(AdapterSelector::Api("Metal".to_string())));
		assert_eq!(parse_adapter_selector("name:4070"), Ok(AdapterSelector::Name("4070".to_string())));
		assert_eq!(parse_adapter_selector("NVIDIA GeForce"), Ok(AdapterSelector::Name("NVIDIA GeForce".to_string())));
		for bad in ["", "  ", "api:", "name: "] {
			assert!(parse_adapter_selector(bad).is_err(), "{bad:?} is refused");
		}
	}

	#[test]
	fn device_modes_parse() {
		assert_eq!(parse_devices("one"), Ok(GpuDevices::One));
		assert_eq!(parse_devices("ALL"), Ok(GpuDevices::All));
		assert_eq!(parse_devices("0,1"), Ok(GpuDevices::List(vec![AdapterSelector::Index(0), AdapterSelector::Index(1)])));
		assert_eq!(parse_devices("name:4070, api:vulkan"), Ok(GpuDevices::List(vec![AdapterSelector::Name("4070".to_string()), AdapterSelector::Api("vulkan".to_string())])));
		for bad in ["", "0,", ",1", "0,,1"] {
			assert!(parse_devices(bad).is_err(), "{bad:?} is refused");
		}
	}

	#[test]
	fn settings_build_the_splimes_config() {
		let one = gpu_config_from_env(Some("1"), None).unwrap().unwrap();
		assert_eq!(one, GpuConfig::default().with_adapter(AdapterSelector::Index(1)));
		let explicit_one = gpu_config_from_env(Some("api:vulkan"), Some("one")).unwrap().unwrap();
		assert_eq!(explicit_one.devices, GpuDevices::One);
		assert_eq!(explicit_one.adapter, Some(AdapterSelector::Api("vulkan".to_string())));
		let all = gpu_config_from_env(None, Some("all")).unwrap().unwrap();
		assert_eq!(all, GpuConfig::default().with_devices(GpuDevices::All));
		assert_eq!(all.max_pool_bytes, GpuConfig::DEFAULT.max_pool_bytes, "the other settings keep splimes' defaults");
		let error = gpu_config_from_env(Some("0"), Some("all")).unwrap_err();
		assert!(error.contains(GPU_ADAPTER_ENV) && error.contains(GPU_DEVICES_ENV), "{error}");
		assert!(gpu_config_from_env(None, Some("0,")).unwrap_err().starts_with(GPU_DEVICES_ENV));
		assert!(gpu_config_from_env(Some("api:"), None).unwrap_err().starts_with(GPU_ADAPTER_ENV));
	}

	#[test]
	fn configurations_are_described() {
		assert_eq!(describe_config(&GpuConfig::default()), "devices: one");
		assert_eq!(describe_config(&GpuConfig::default().with_adapter(AdapterSelector::Name("4070".to_string()))), "devices: one (adapter name \"4070\")");
		assert_eq!(describe_config(&GpuConfig::default().with_devices(GpuDevices::All)), "devices: all");
		assert_eq!(describe_config(&GpuConfig::default().with_devices(GpuDevices::List(vec![AdapterSelector::Index(0), AdapterSelector::Api("vulkan".to_string())]))), "devices: list [#0, api \"vulkan\"]");
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
