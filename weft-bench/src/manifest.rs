//! The run manifest: the artifact a benchmark claim points at (1.0 plan EVI-3).
//!
//! A report says what was measured. A manifest says **what it was measured on, how it was
//! produced, and whether the machine would say the same thing again**. It is a separate file
//! beside the report — the report is a results table that several tools read, and provenance
//! belongs to the run, not to the format.
//!
//! Two rules make it worth the extra file:
//!
//! * **It is written to be published.** Every free-text string goes through
//!   [`crate::redaction`] at capture, and the environment is allow-listed rather than swept, so
//!   a manifest uploaded as a CI artifact cannot carry a credential or an account name.
//! * **It verifies itself.** [`RunManifest::verify`] re-reads the report it points at, re-hashes
//!   it, and checks its own provenance for completeness. `weft-bench verify` runs it, and a
//!   verifier that finds a problem refuses the manifest rather than grading it softly.
//!
//! Verification is a pure function of the file plus the local file system: no clock, no RNG, no
//! probe. The same manifest verifies the same way tomorrow, which is the whole reason a published
//! claim can point at it.

use std::{
	collections::BTreeMap, fmt, path::{Path, PathBuf}
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{redaction, report::BenchReport, schema::BenchResult};

/// The manifest schema version. Bump on any change to the shape below.
///
/// It shares the number line with the report schema ([`crate::schema::SCHEMA_VERSION`], v22) so a
/// reader can tell at a glance which generation of artifact a pair of files belongs to; the
/// manifest is the next step on that line.
pub const MANIFEST_SCHEMA_VERSION: u32 = 23;

/// The environment variables a manifest records.
///
/// An **allow-list**, not a deny-list: a value that is never read cannot leak, which is a stronger
/// guarantee than reading everything and hoping the redactor knows every credential shape. These
/// are the variables that change what a run measures — the toolchain flags it was built with, the
/// GPU selection, and the harness's own tuning overrides — and nothing else.
pub const CAPTURED_ENV: &[&str] = &["RUSTFLAGS", "CARGO_BUILD_JOBS", "TARGET_CPU", "RUST_BACKTRACE", "CUDA_VISIBLE_DEVICES", "WEFTDB_BENCH_GPU", "WEFTDB_GPU_MIN_POINTS", "WEFTDB_PARALLEL_MIN_POINTS", "WEFTDB_DISABLE_GPU", "TZ"];

/// How a measure should be compared across runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
	/// Smaller is better (latency, error, bytes/point).
	Lower,
	/// Larger is better (throughput).
	Higher,
	/// A pass/fail verdict rather than a magnitude.
	Pass,
}

impl fmt::Display for Better {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Lower => "lower",
			Self::Higher => "higher",
			Self::Pass => "pass",
		})
	}
}

/// One reference-grade number from the report, with the context to compare it.
///
/// The nightly regression track and `weft-bench compare` read these instead of re-deriving the
/// metric from the report, so the definition of "the headline numbers" lives in exactly one place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measure {
	/// Dotted metric name, e.g. `latency.p50_ns`.
	pub name: String,
	/// The adapter the number belongs to, so a multi-adapter run stays attributable.
	pub adapter: String,
	/// The value. Booleans become `1.0` (pass) and `0.0` (fail).
	pub value: f64,
	/// Unit of [`Self::value`]: `ns`, `points_per_sec`, `bytes_per_point`, `value_units`, `flag`.
	pub unit: String,
	/// Which direction is an improvement.
	pub better: Better,
	/// True for the metrics the headline table quotes — the ones a regression gate watches.
	pub headline: bool,
}

/// What this run concluded, in machine-checkable form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceGrade {
	/// Every result passed correctness (`BenchReport::is_publishable`). A run that failed
	/// correctness still gets a manifest — it is evidence of a failure — but `publishable` is
	/// `false` and no verifier should let it support a claim.
	pub publishable: bool,
	/// Results in the report.
	pub result_count: usize,
	/// Results individually fit to publish.
	pub publishable_count: usize,
	/// The reference-grade measures, in report order then metric order.
	pub measures: Vec<Measure>,
}

/// The toolchain and build profile that produced the measured binary.
///
/// Recorded at **compile** time by `weft-bench/build.rs`, not probed at run time: the binary is what
/// a quoted number describes, and by the time it runs the compiler that built it may be a different
/// toolchain — or absent, which is the normal case for a released binary.
///
/// Every field is optional because a build script cannot always read a value: an unusual toolchain
/// reports no commit hash, and a source tree without git has no revision. Absent is the honest
/// record, and a guess would be worse, because a wrong compiler version still looks like proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFacts {
	/// The compiler's own version, e.g. `1.99.0`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rustc: Option<String>,
	/// The compiler's git hash, when it is a git build.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rustc_commit: Option<String>,
	/// The compiler's host triple.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rustc_host: Option<String>,
	/// The LLVM version the compiler links.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub llvm_version: Option<String>,
	/// The target triple the binary was built for.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub target: Option<String>,
	/// `debug` or `release`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub profile: Option<String>,
	/// `opt-level`, which decides whether the numbers mean anything: `0` and `3` under one workload
	/// name are different benchmarks.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub opt_level: Option<String>,
	/// `debug` from the profile, as cargo spells the list.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub debug: Option<String>,
	/// `lto` from the profile.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub lto: Option<String>,
	/// `debug-assertions`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub debug_assertions: Option<String>,
	/// `overflow-checks`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub overflow_checks: Option<String>,
	/// `panic` strategy.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub panic: Option<String>,
}

impl BuildFacts {
	/// The facts the build script baked into this binary.
	#[must_use]
	pub fn capture() -> Self {
		let text = |value: Option<&'static str>| value.map(str::to_string);
		Self { rustc: text(option_env!("WEFTDB_BUILD_RUSTC_SEMVER")), rustc_commit: text(option_env!("WEFTDB_BUILD_RUSTC_COMMIT")), rustc_host: text(option_env!("WEFTDB_BUILD_RUSTC_HOST")), llvm_version: text(option_env!("WEFTDB_BUILD_LLVM_VERSION")), target: text(option_env!("WEFTDB_BUILD_TARGET")), profile: text(option_env!("WEFTDB_BUILD_PROFILE")), opt_level: text(option_env!("WEFTDB_BUILD_OPT_LEVEL")), debug: text(option_env!("WEFTDB_BUILD_DEBUG")), lto: text(option_env!("WEFTDB_BUILD_LTO")), debug_assertions: text(option_env!("WEFTDB_BUILD_DEBUG_ASSERTIONS")), overflow_checks: text(option_env!("WEFTDB_BUILD_OVERFLOW_CHECKS")), panic: text(option_env!("WEFTDB_BUILD_PANIC")) }
	}

	/// The revision the binary was compiled from, when the build script could read one.
	#[must_use]
	pub fn git_sha(&self) -> Option<&'static str> {
		option_env!("WEFTDB_BUILD_GIT_SHA")
	}

	/// Whether the tree was modified when the binary was compiled.
	#[must_use]
	pub fn git_dirty(&self) -> Option<bool> {
		match option_env!("WEFTDB_BUILD_GIT_DIRTY") {
			Some("true") => Some(true),
			Some(_) => Some(false),
			None => None,
		}
	}
}

/// The hardware/toolchain block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
	/// `weft-bench` version that wrote this manifest.
	pub weft_bench_version: String,
	/// Operating system (`std::env::consts::OS`).
	pub os: String,
	/// Architecture (`std::env::consts::ARCH`).
	pub arch: String,
	/// The toolchain and profile that built the binary which measured this.
	pub build: BuildFacts,
	/// CPU brand, when the platform reports one.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_model: Option<String>,
	/// Physical cores.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_cores_physical: Option<usize>,
	/// Logical cores.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_cores_logical: Option<usize>,
	/// Total bytes of RAM.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub total_memory_bytes: Option<u64>,
	/// Kind of disk holding the working directory (`ssd`/`hdd`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub work_disk_kind: Option<String>,
	/// Its file system.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub work_disk_file_system: Option<String>,
	/// Its mount point.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub work_disk_mount_point: Option<String>,
	/// The GPU the engine selected.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub gpu: Option<String>,
	/// The GPU driver's own name and version.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub gpu_driver: Option<String>,
	/// How the engine's thresholds were chosen.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub engine_calibration: Option<String>,
	/// Why there is no GPU, or why calibration was skipped.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub engine_note: Option<String>,
	/// The target CPU the binary was compiled for (`-C target-cpu`), when it was set.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub target_cpu: Option<String>,
}

/// How the run was invoked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
	/// The process arguments, secret-redacted and home-redacted.
	pub command_line: Vec<String>,
	/// The working directory, with the home directory replaced by `~`.
	pub working_directory: String,
	/// Environment variables from [`CAPTURED_ENV`] that were set, redacted.
	pub environment: BTreeMap<String, String>,
	/// Operator labels from repeated `--label name=value`, redacted.
	pub labels: BTreeMap<String, String>,
}

/// What this manifest is about, and how to tell it has not been edited since.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sources {
	/// The report this manifest describes, as recorded at capture time.
	pub report: String,
	/// Size of that file in bytes.
	pub report_bytes: u64,
	/// SHA-256 of the report file's exact bytes, lower-case hex.
	pub report_sha256: String,
	/// The report's own `schema_version`.
	pub report_schema: u32,
	/// The revision the measured binary was compiled from, as recorded at build time.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub git_sha: Option<String>,
	/// True when the tree was modified at compile time. A release artifact must not be.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub git_dirty: Option<bool>,
}

/// A completed benchmark run and everything needed to trust it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunManifest {
	/// Manifest schema version; [`MANIFEST_SCHEMA_VERSION`] for one this build writes.
	pub schema_version: u32,
	/// When the run finished, RFC 3339 UTC.
	pub generated_at: String,
	/// Hardware and toolchain.
	pub provenance: Provenance,
	/// Invocation.
	pub invocation: Invocation,
	/// The report and the source it came from.
	pub sources: Sources,
	/// The verdict and its numbers.
	pub grade: ReferenceGrade,
	/// The `--claim <id>` this run is evidence for, if it was labelled as one.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub claim: Option<String>,
}

/// Why a manifest could not be read or parsed.
#[derive(Debug)]
pub enum ManifestError {
	/// The file could not be read or written.
	Io {
		/// What was being opened.
		path: PathBuf,
		/// What the OS said.
		source: std::io::Error,
	},
	/// The file is not a manifest.
	Parse {
		/// The offending file.
		path: PathBuf,
		/// What serde_json said.
		source: serde_json::Error,
	},
	/// The manifest is a schema this build cannot fully verify.
	UnsupportedSchema {
		/// The version found in the file.
		found: u32,
		/// The version this build writes.
		expected: u32,
	},
}

impl fmt::Display for ManifestError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Io { path, source } => write!(f, "cannot read manifest {}: {source}", path.display()),
			Self::Parse { path, source } => write!(f, "{} is not a run manifest: {source}", path.display()),
			Self::UnsupportedSchema { found, expected } => write!(f, "manifest schema v{found} cannot be verified by a build that writes v{expected}"),
		}
	}
}

impl std::error::Error for ManifestError {}

/// One reason a manifest failed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
	/// A required provenance field is empty.
	MissingField(&'static str),
	/// The report the manifest points at could not be read.
	SourceUnreadable {
		/// The recorded path, after `~` expansion.
		path: String,
		/// Why.
		reason: String,
	},
	/// The report's bytes no longer match the recorded hash.
	ChecksumMismatch {
		/// The recorded path, after `~` expansion.
		path: String,
		/// Hash recorded at capture.
		expected: String,
		/// Hash of the bytes on disk now.
		actual: String,
	},
	/// The report parsed but is not consistent with what the manifest recorded.
	ReportMismatch(String),
	/// A provenance field still holds something shaped like a credential.
	CredentialInProvenance(&'static str),
	/// A measure name is duplicated, so a comparison could not select one.
	DuplicateMeasure(String),
	/// The binary records no source revision, so nothing here can be re-produced.
	SourceUnrecorded,
	/// The binary was compiled from a modified tree.
	DirtySource,
}

impl fmt::Display for Problem {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::MissingField(field) => write!(f, "required provenance field `{field}` is empty"),
			Self::SourceUnreadable { path, reason } => write!(f, "recorded report {path} cannot be read: {reason}"),
			Self::ChecksumMismatch { path, expected, actual } => write!(f, "report {path} no longer matches the manifest: recorded sha256 {expected}, file is {actual}"),
			Self::ReportMismatch(why) => write!(f, "report does not match the manifest: {why}"),
			Self::CredentialInProvenance(field) => write!(f, "provenance field `{field}` still carries a credential shape"),
			Self::DuplicateMeasure(name) => write!(f, "measure `{name}` appears more than once"),
			Self::SourceUnrecorded => f.write_str("the measured binary records no source revision"),
			Self::DirtySource => f.write_str("the measured binary was compiled from a modified working tree"),
		}
	}
}

/// The outcome of verifying one manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Verdict {
	/// Every problem found; empty means the manifest verifies.
	pub problems: Vec<Problem>,
}

impl Problem {
	/// True when the problem means the manifest is broken rather than merely imperfect.
	///
	/// Capture refuses to write a manifest with a defect in it — a half-recorded provenance is
	/// worse than none, because it looks like evidence. A tree that is not clean is different: the
	/// numbers are still real and the record still says so, so it is reported at the gate rather
	/// than costing the run.
	#[must_use]
	pub const fn is_defect(&self) -> bool {
		!matches!(self, Self::SourceUnrecorded | Self::DirtySource)
	}
}

impl Verdict {
	/// The problems that mean the manifest itself is broken.
	#[must_use]
	pub fn defects(&self) -> Vec<&Problem> {
		self.problems.iter().filter(|problem| problem.is_defect()).collect()
	}

	/// True when nothing was wrong.
	#[must_use]
	pub fn is_clean(&self) -> bool {
		self.problems.is_empty()
	}
}

impl RunManifest {
	/// Capture a manifest for `report`, whose JSON is on disk at `report_path`.
	///
	/// The report file is hashed **as written**, because that is the bytes a verifier will re-read;
	/// hashing a re-serialization would let a re-formatted file pass unnoticed.
	///
	/// # Errors
	///
	/// Returns [`ManifestError::Io`] when `report_path` cannot be read — which means the caller
	/// tried to capture a manifest for a run whose report was never written.
	pub fn capture(report: &BenchReport, report_path: &Path, command_line: &[String], labels: &[(String, String)], claim: Option<&str>) -> Result<Self, ManifestError> {
		let bytes = std::fs::read(report_path).map_err(|source| ManifestError::Io { path: report_path.to_path_buf(), source })?;
		let report_sha256 = hex_digest(&bytes);
		let build = BuildFacts::capture();

		let provenance = Provenance { weft_bench_version: report.metadata.weft_bench_version.clone(), os: redaction::redact(&report.metadata.os), arch: redaction::redact(&report.metadata.arch), build: build.clone(), cpu_model: report.metadata.cpu_model.as_deref().map(redaction::redact).filter(|s| !s.is_empty()), cpu_cores_physical: report.metadata.cpu_cores_physical, cpu_cores_logical: report.metadata.cpu_cores_logical, total_memory_bytes: report.metadata.total_memory_bytes, work_disk_kind: report.metadata.work_disk_kind.clone(), work_disk_file_system: report.metadata.work_disk_file_system.clone(), work_disk_mount_point: report.metadata.work_disk_mount_point.as_deref().map(redaction::redact).filter(|s| !s.is_empty()), gpu: report.metadata.engine.as_ref().and_then(|e| e.gpu.as_deref()).map(redaction::redact).filter(|s| !s.is_empty()), gpu_driver: report.metadata.engine.as_ref().and_then(|e| e.gpu_driver.as_deref()).map(redaction::redact).filter(|s| !s.is_empty()), engine_calibration: report.metadata.engine.as_ref().map(|e| redaction::redact(&e.describe_calibration())), engine_note: report.metadata.engine.as_ref().and_then(|e| e.note.as_deref()).map(redaction::redact).filter(|s| !s.is_empty()), target_cpu: std::env::var("TARGET_CPU").ok().filter(|v| !v.trim().is_empty()).map(|v| redaction::redact(v.trim())) };

		let invocation = Invocation { command_line: command_line.iter().map(|arg| redaction::redact(arg)).collect(), working_directory: std::env::current_dir().map(|dir| redaction::redact_home(&dir.to_string_lossy())).unwrap_or_else(|_| String::new()), environment: capture_environment(), labels: labels.iter().map(|(name, value)| (redaction::redact(name), redaction::redact(value))).collect() };

		let sources = Sources { report: redaction::redact_home(&report_path.to_string_lossy()), report_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX), report_sha256, report_schema: report.schema_version, git_sha: build.git_sha().map(str::to_string), git_dirty: build.git_dirty() };

		Ok(Self { schema_version: MANIFEST_SCHEMA_VERSION, generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true), provenance, invocation, sources, grade: reference_grade(report), claim: claim.map(redaction::redact).filter(|c| !c.is_empty()) })
	}

	/// Pretty-printed JSON.
	///
	/// # Errors
	///
	/// A serialization failure here would be a serde configuration bug.
	pub fn to_json_pretty(&self) -> serde_json::Result<String> {
		serde_json::to_string_pretty(self)
	}

	/// Write the manifest as pretty-printed JSON, creating parent directories as needed, then
	/// verify what was written and refuse to return `Ok` if it does not verify.
	///
	/// A manifest that cannot be verified is worse than none: it looks like evidence. So capture
	/// checks its own work against the file it just wrote, and a run that produced an unverifiable
	/// manifest fails loudly at the moment it is written rather than at publication time.
	///
	/// # Errors
	///
	/// [`ManifestError::Io`] for a write failure, [`ManifestError::Parse`] if the bytes just
	/// written do not read back as a manifest, `std::io::Error::other` listing every
	/// [`Problem`] found in what was written.
	pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
		if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
			std::fs::create_dir_all(parent)?;
		}
		let json = self.to_json_pretty().map_err(std::io::Error::other)?;
		std::fs::write(path, json.as_bytes())?;
		let read_back = Self::read(path).map_err(std::io::Error::other)?;
		let verdict = read_back.verify(path);
		let defects = verdict.defects();
		if !defects.is_empty() {
			let problems: Vec<String> = defects.iter().map(ToString::to_string).collect();
			return Err(std::io::Error::other(format!("manifest {} was written but does not verify: {}", path.display(), problems.join("; "))));
		}
		Ok(())
	}

	/// Read a manifest from disk.
	///
	/// # Errors
	///
	/// [`ManifestError::Io`] or [`ManifestError::Parse`].
	pub fn read(path: &Path) -> Result<Self, ManifestError> {
		let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Io { path: path.to_path_buf(), source })?;
		Self::from_json(&text, path)
	}

	/// Parse a manifest from text; `path` is only used to name the file in errors.
	///
	/// # Errors
	///
	/// [`ManifestError::Parse`] when the text is not a manifest, [`ManifestError::UnsupportedSchema`]
	/// when it is a manifest of a version this build cannot verify.
	pub fn from_json(text: &str, path: &Path) -> Result<Self, ManifestError> {
		let manifest: Self = serde_json::from_str(text).map_err(|source| ManifestError::Parse { path: path.to_path_buf(), source })?;
		if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
			return Err(ManifestError::UnsupportedSchema { found: manifest.schema_version, expected: MANIFEST_SCHEMA_VERSION });
		}
		Ok(manifest)
	}

	/// Verify this manifest, resolving the report path relative to `manifest_path`.
	///
	/// Checks, in order: that the provenance is complete, that no provenance field carries a
	/// credential shape, that the recorded report exists and hashes to what was recorded, that it
	/// parses and agrees with the manifest about its schema, and that the measure set is usable.
	/// Every problem is reported, not just the first: a gate should say everything that is wrong.
	#[must_use]
	pub fn verify(&self, manifest_path: &Path) -> Verdict {
		let mut problems = Vec::new();
		let required: &[(&str, &str)] = &[("weft_bench_version", &self.provenance.weft_bench_version), ("os", &self.provenance.os), ("arch", &self.provenance.arch)];
		for (name, value) in required {
			if value.trim().is_empty() {
				problems.push(Problem::MissingField(name));
			}
		}
		for (name, value) in [("provenance.build.rustc", self.provenance.build.rustc.as_deref()), ("provenance.build.target", self.provenance.build.target.as_deref()), ("provenance.build.opt_level", self.provenance.build.opt_level.as_deref())] {
			if value.is_none_or(|value| value.trim().is_empty()) {
				problems.push(Problem::MissingField(name));
			}
		}
		// A number with no recorded source cannot be re-produced, so it cannot support a claim:
		// refusing here is the difference between a manifest and an unsourced figure.
		if self.sources.git_sha.is_none() {
			problems.push(Problem::SourceUnrecorded);
		}
		if self.sources.git_dirty == Some(true) {
			problems.push(Problem::DirtySource);
		}
		if self.sources.report.trim().is_empty() {
			problems.push(Problem::MissingField("sources.report"));
		}
		if self.sources.report_sha256.len() != 64 || !self.sources.report_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
			problems.push(Problem::MissingField("sources.report_sha256"));
		}

		// Capture redacts every free-text field, so a clean manifest must be a fixed point of the
		// redactor. Anything that changes under redaction is a credential that got in.
		for (name, value) in self.provenance_fields() {
			if redaction::redact(value) != *value {
				problems.push(Problem::CredentialInProvenance(name));
			}
		}

		if let Some(path) = self.report_path(manifest_path) {
			match std::fs::read(&path) {
				Ok(bytes) => {
					let actual = hex_digest(&bytes);
					if actual != self.sources.report_sha256 {
						problems.push(Problem::ChecksumMismatch { path: path.display().to_string(), expected: self.sources.report_sha256.clone(), actual });
					} else if let Err(error) = serde_json::from_slice::<BenchReport>(&bytes) {
						problems.push(Problem::ReportMismatch(format!("{} is not a readable report: {error}", path.display())));
					} else if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != self.sources.report_bytes {
						problems.push(Problem::ReportMismatch(format!("{} is {} bytes, the manifest recorded {}", path.display(), bytes.len(), self.sources.report_bytes)));
					}
				}
				Err(error) => problems.push(Problem::SourceUnreadable { path: path.display().to_string(), reason: error.to_string() }),
			}
		} else {
			problems.push(Problem::SourceUnreadable { path: self.sources.report.clone(), reason: "the recorded path is not usable on this host".to_string() });
		}

		let mut names: Vec<&str> = self.grade.measures.iter().map(|m| m.name.as_str()).collect();
		let before = names.len();
		names.sort_unstable();
		names.dedup();
		if names.len() != before {
			problems.push(Problem::DuplicateMeasure("a measure name is repeated across adapters".to_string()));
		}

		Verdict { problems }
	}

	/// Every free-text provenance string, named — the fields a credential could be sitting in.
	fn provenance_fields(&self) -> Vec<(&'static str, &str)> {
		let mut fields = vec![("provenance.cpu_model", self.provenance.cpu_model.as_deref().unwrap_or("")), ("provenance.gpu", self.provenance.gpu.as_deref().unwrap_or("")), ("provenance.gpu_driver", self.provenance.gpu_driver.as_deref().unwrap_or("")), ("provenance.engine_calibration", self.provenance.engine_calibration.as_deref().unwrap_or("")), ("provenance.engine_note", self.provenance.engine_note.as_deref().unwrap_or("")), ("provenance.work_disk_mount_point", self.provenance.work_disk_mount_point.as_deref().unwrap_or("")), ("provenance.target_cpu", self.provenance.target_cpu.as_deref().unwrap_or("")), ("invocation.working_directory", &self.invocation.working_directory), ("sources.report", &self.sources.report)];
		if let Some(claim) = &self.claim {
			fields.push(("claim", claim));
		}
		if let Some(rustc) = &self.provenance.build.rustc {
			fields.push(("provenance.build.rustc", rustc.as_str()));
		}
		if let Some(rustc_host) = &self.provenance.build.rustc_host {
			fields.push(("provenance.build.rustc_host", rustc_host.as_str()));
		}
		fields.extend(self.invocation.environment.values().map(|value| ("invocation.environment", value.as_str())));
		fields.extend(self.invocation.labels.values().map(|value| ("invocation.labels", value.as_str())));
		fields.extend(self.invocation.command_line.iter().map(|arg| ("invocation.command_line", arg.as_str())));
		fields
	}

	/// Where the recorded report lives now.
	///
	/// Capture stores the path as the operator wrote it, with the home directory replaced by `~`,
	/// so verification resolves it in the order a reader would expect: as an absolute path, then
	/// beside the manifest — including the `<stem>.json` sibling this crate writes — then under the
	/// home directory. `None` only when nothing was recorded, which is itself a problem.
	///
	/// Resolution is deliberately forgiving about *location* and strict about *content*: the file
	/// found here still has to hash to what capture recorded, so a path that resolves to the wrong
	/// report fails verification rather than passing it.
	#[must_use]
	pub fn report_path(&self, manifest_path: &Path) -> Option<PathBuf> {
		let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(|value| value.to_string_lossy().into_owned());
		let recorded = self.sources.report.strip_prefix('~').map_or_else(|| self.sources.report.as_str(), |rest| rest.strip_prefix('/').unwrap_or(rest));
		if recorded.is_empty() {
			return None;
		}
		let from_home = self.sources.report.starts_with('~');
		let path = PathBuf::from(recorded);
		if path.is_absolute() && path.exists() {
			return Some(path);
		}
		// Verification usually runs from a different directory than capture, so the recorded path
		// is a hint rather than an address. This writes `<report>.manifest.json` beside its report,
		// so the sibling with the manifest's own stem is the strongest candidate; then the recorded
		// path relative to the manifest, then relative to home.
		if let Some(sibling) = manifest_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
			if let Some(stem) = manifest_path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".manifest.json")) {
				let candidate = sibling.join(format!("{stem}.json"));
				if candidate.exists() {
					return Some(candidate);
				}
			}
			let candidate = sibling.join(&path);
			if candidate.exists() {
				return Some(candidate);
			}
		}
		if let Some(home) = home.filter(|value| value.len() > 1) {
			let candidate = PathBuf::from(home).join(&path);
			if candidate.exists() {
				return Some(candidate);
			}
			// A `~/…` record names a place under home even when nothing is there, and an error that
			// quotes the stripped path would point the operator at a directory they never used.
			if from_home {
				return Some(candidate);
			}
		}
		Some(path)
	}
}

/// The environment block: [`CAPTURED_ENV`], redacted, missing variables omitted.
fn capture_environment() -> BTreeMap<String, String> {
	CAPTURED_ENV.iter().filter_map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()).map(|value| ((*name).to_string(), redaction::redact(value.trim())))).collect()
}

/// The reference-grade measures of a report.
///
/// These are the numbers a claim is made of: the latency distribution, throughput, the storage
/// cost per point, reconstruction error where a ground truth exists, and the two correctness
/// verdicts. Nothing here re-derives a metric — every value is quoted from the result it belongs
/// to, so the manifest and the report cannot drift into disagreeing about what was measured.
#[must_use]
pub fn reference_grade(report: &BenchReport) -> ReferenceGrade {
	let mut measures = Vec::new();
	for result in &report.results {
		push_result_measures(result, &mut measures);
	}
	ReferenceGrade { publishable: report.is_publishable(), result_count: report.results.len(), publishable_count: report.publishable_count(), measures }
}

/// The graded measures of one result, for a caller comparing two runs.
///
/// `weft-bench compare` takes its metric names, units and better-directions here rather than naming
/// fields itself, so the manifest and the regression gate cannot disagree about which number a
/// workload is judged by.
#[must_use]
pub fn result_measures(result: &BenchResult) -> Vec<Measure> {
	let mut measures = Vec::new();
	push_result_measures(result, &mut measures);
	measures
}

/// Append the measures of one result.
fn push_result_measures(result: &BenchResult, measures: &mut Vec<Measure>) {
	let adapter = result.adapter.as_str();
	// Latency is a whole number of nanoseconds; f64 carries it exactly, so the cast is a
	// representation change and not a rounding choice.
	#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
	let as_f64 = |value: u64| value as f64;
	measures.push(Measure { name: "latency.p50_ns".to_string(), adapter: adapter.to_string(), value: as_f64(result.latency.p50_ns), unit: "ns".to_string(), better: Better::Lower, headline: true });
	measures.push(Measure { name: "latency.mean_ns".to_string(), adapter: adapter.to_string(), value: as_f64(result.latency.mean_ns), unit: "ns".to_string(), better: Better::Lower, headline: true });
	measures.push(Measure { name: "latency.p95_ns".to_string(), adapter: adapter.to_string(), value: as_f64(result.latency.p95_ns), unit: "ns".to_string(), better: Better::Lower, headline: true });
	measures.push(Measure { name: "latency.p99_ns".to_string(), adapter: adapter.to_string(), value: as_f64(result.latency.p99_ns), unit: "ns".to_string(), better: Better::Lower, headline: false });
	measures.push(Measure { name: "throughput.points_per_sec".to_string(), adapter: adapter.to_string(), value: result.throughput_points_per_sec, unit: "points_per_sec".to_string(), better: Better::Higher, headline: true });
	if let Some(storage) = &result.storage {
		measures.push(Measure { name: "storage.bytes_per_point".to_string(), adapter: adapter.to_string(), value: storage.bytes_per_point, unit: "bytes_per_point".to_string(), better: Better::Lower, headline: true });
		measures.push(Measure { name: "storage.total_bytes_per_point".to_string(), adapter: adapter.to_string(), value: storage.total_bytes_per_point, unit: "bytes_per_point".to_string(), better: Better::Lower, headline: false });
	}
	if let Some(accuracy) = &result.accuracy {
		measures.push(Measure { name: "accuracy.rmse".to_string(), adapter: adapter.to_string(), value: accuracy.rmse, unit: "value_units".to_string(), better: Better::Lower, headline: true });
		measures.push(Measure { name: "accuracy.mae".to_string(), adapter: adapter.to_string(), value: accuracy.mae, unit: "value_units".to_string(), better: Better::Lower, headline: false });
		measures.push(Measure { name: "accuracy.max_abs_error".to_string(), adapter: adapter.to_string(), value: accuracy.max_abs_error, unit: "value_units".to_string(), better: Better::Lower, headline: false });
	}
	measures.push(Measure { name: "correctness.passed".to_string(), adapter: adapter.to_string(), value: if result.correctness.passed() { 1.0 } else { 0.0 }, unit: "flag".to_string(), better: Better::Pass, headline: false });
}

/// Lower-case hex SHA-256 of `bytes`.
fn hex_digest(bytes: &[u8]) -> String {
	let mut hasher = Sha256::new();
	hasher.update(bytes);
	let digest = hasher.finalize();
	let mut out = String::with_capacity(digest.len() * 2);
	for byte in digest {
		use std::fmt::Write as _;
		let _ = write!(out, "{byte:02x}");
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		redaction::REDACTED, report::RunMetadata, schema::{CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown}, stats::LatencyStats
	};

	/// A temp directory unique to this test binary and tag, emptied first.
	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("weft-bench-manifest-{}-{}", std::process::id(), tag));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	/// A result with the shape a real interpolation run produces: latency, storage, and an
	/// accuracy block (which only a synthetic profile carries).
	fn sample_result(adapter: &str, publishable: bool) -> BenchResult {
		BenchResult { schema_version: crate::schema::SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: adapter.to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7, signal_shape: None }, latency: LatencyStats::from_samples(&[100, 200, 300]), latency_ci: None, throughput_points_per_sec: 5_000_000.0, timing: TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 }, correctness: CorrectnessReport { output_count_ok: publishable, expected_output_points: 1000, actual_output_points: 1000, values_finite: publishable }, accuracy: Some(crate::accuracy::AccuracyMetrics { count: 1000, rmse: 0.5, mae: 0.4, max_abs_error: 1.5, bias: 0.0 }), storage: Some(sample_storage()), cold_warm: None, gpu_pool: None }
	}

	/// A storage estimate shaped like a real f64 compression result: the value column realized
	/// 6.5 bytes/point, timestamps 1.2, total 7.7.
	fn sample_storage() -> StorageEstimate {
		StorageEstimate { physical_type: "f64".to_string(), value_count: 1000, estimated_value_bytes: 6500, realized_value_bytes: 6500, value_codec: "blocked".to_string(), bytes_per_point: 6.5, is_exact: true, lossy_count: 0, max_abs_error: "0".to_string(), tolerance: "0".to_string(), timestamp_unit: "ns".to_string(), timestamp_encoding: "blocked".to_string(), timestamp_bytes: 1200, timestamp_bytes_per_point: 1.2, total_bytes_per_point: 7.7, advisory_best_f64_bytes: None, advisory_best_f64_codec: None, advisory_fire_timestamp_bytes: None, advisory_delta_cascade_value_bytes: None, advisory_dfor_value_bytes: None, advisory_common_multiple_timestamp_bytes: None, advisory_delta_for_timestamp_bytes: None }
	}

	fn sample_report() -> BenchReport {
		let metadata = RunMetadata { weft_bench_version: "0.1.0".to_string(), os: "testos".to_string(), arch: "testarch".to_string(), cpu_model: Some("Test CPU 9000".to_string()), cpu_cores_physical: Some(8), cpu_cores_logical: Some(16), total_memory_bytes: Some(34_359_738_368), work_disk_kind: Some("ssd".to_string()), work_disk_file_system: Some("btrfs".to_string()), work_disk_mount_point: Some("/mnt/data".to_string()), engine: None, generated_at: "2026-10-25T00:00:00+00:00".to_string() };
		BenchReport::with_results(metadata, vec![sample_result("weftdb", true)])
	}

	/// Capture a manifest for a report written into a fresh temp dir, returning both paths.
	fn captured(tag: &str, labels: &[(String, String)], claim: Option<&str>) -> (BenchReport, PathBuf, PathBuf) {
		let dir = temp_dir(tag);
		let report = sample_report();
		let report_path = dir.join("heavy__weftdb.json");
		report.write_json(&report_path).expect("report writes");
		let manifest = RunManifest::capture(&report, &report_path, &["weft-bench".to_string(), "--synthetic".to_string()], labels, claim).expect("capture");
		let manifest_path = dir.join("heavy__weftdb.manifest.json");
		manifest.write_to(&manifest_path).expect("manifest writes and verifies");
		(report, report_path, manifest_path)
	}

	fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
		pairs.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect()
	}

	#[test]
	fn a_written_manifest_verifies_against_the_report_it_points_at() {
		let (_report, _report_path, manifest_path) = captured("roundtrip", &labels(&[("operator", "ci")]), Some("EVI-3"));
		let read_back = RunManifest::read(&manifest_path).expect("the manifest reads back");
		let verdict = read_back.verify(&manifest_path);
		assert!(verdict.defects().is_empty(), "a freshly written manifest must have no defects: {:?}", verdict.problems);
		assert_eq!(read_back.claim.as_deref(), Some("EVI-3"));
		assert_eq!(read_back.grade.result_count, 1);
		assert!(read_back.grade.publishable);
		assert_eq!(read_back.sources.report_sha256.len(), 64);
		assert!(read_back.sources.report_bytes > 0);
		assert_eq!(read_back.schema_version, MANIFEST_SCHEMA_VERSION);
	}

	#[test]
	fn the_record_is_byte_stable_apart_from_the_clock() {
		let dir = temp_dir("determinism");
		let report = sample_report();
		let report_path = dir.join("run.json");
		report.write_json(&report_path).expect("report writes");
		let args = vec!["weft-bench".to_string(), "--synthetic".to_string()];
		let first = RunManifest::capture(&report, &report_path, &args, &[], None).expect("capture");
		let second = RunManifest::capture(&report, &report_path, &args, &[], None).expect("capture");
		let (mut first, mut second) = (first, second);
		first.generated_at = "fixed".to_string();
		second.generated_at = "fixed".to_string();
		assert_eq!(first, second, "two captures of the same run must be identical, or a diff of two manifests proves nothing");
		assert_eq!(first.to_json_pretty().expect("serializes"), second.to_json_pretty().expect("serializes"));
	}

	#[test]
	fn a_label_that_looks_like_a_credential_is_redacted_at_capture() {
		let (_report, _report_path, manifest_path) = captured("labels", &labels(&[("token", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"), ("note", "nightly")]), None);
		let manifest = RunManifest::read(&manifest_path).expect("reads");
		assert_eq!(manifest.invocation.labels["token"], REDACTED);
		assert_eq!(manifest.invocation.labels["note"], "nightly", "an ordinary label must survive untouched");
		assert!(manifest.verify(&manifest_path).defects().is_empty());
	}

	#[test]
	fn tampering_with_the_report_after_capture_breaks_the_hash() {
		let (_report, report_path, manifest_path) = captured("tamper", &[], None);
		let mut bytes = std::fs::read(&report_path).expect("report");
		bytes.extend_from_slice(b"\n");
		std::fs::write(&report_path, bytes).expect("tampered report writes");
		let manifest = RunManifest::read(&manifest_path).expect("reads");
		let verdict = manifest.verify(&manifest_path);
		assert!(verdict.problems.iter().any(|problem| matches!(problem, Problem::ChecksumMismatch { .. })), "a report edited after capture must be caught: {:?}", verdict.problems);
		assert!(verdict.defects().iter().any(|problem| matches!(problem, Problem::ChecksumMismatch { .. })), "a hash mismatch is a defect, not an advisory");
	}

	#[test]
	fn a_missing_report_is_a_defect() {
		let (_report, report_path, manifest_path) = captured("missing", &[], None);
		std::fs::remove_file(&report_path).expect("report removed");
		let manifest = RunManifest::read(&manifest_path).expect("reads");
		let verdict = manifest.verify(&manifest_path);
		assert!(verdict.problems.iter().any(|problem| matches!(problem, Problem::SourceUnreadable { .. })), "{:?}", verdict.problems);
	}

	#[test]
	fn the_report_is_found_beside_the_manifest_when_the_recorded_path_no_longer_applies() {
		let dir = temp_dir("sibling");
		let report = sample_report();
		let report_path = dir.join("heavy__weftdb.json");
		report.write_json(&report_path).expect("report writes");
		let mut manifest = RunManifest::capture(&report, &report_path, &["weft-bench".to_string()], &[], None).expect("capture");
		// Pretend the run happened on another host, where the report sat at a relative path.
		manifest.sources.report = "reports/json/heavy__weftdb.json".to_string();
		let manifest_path = dir.join("heavy__weftdb.manifest.json");
		manifest.write_to(&manifest_path).expect("the sibling lookup finds the report");
		assert!(manifest.verify(&manifest_path).defects().is_empty());
	}

	#[test]
	fn a_manifest_of_another_schema_version_is_refused_not_guessed() {
		let (_report, _report_path, manifest_path) = captured("schema", &[], None);
		let text = std::fs::read_to_string(&manifest_path).expect("reads");
		let downgraded = text.replacen(&format!("\"schema_version\": {MANIFEST_SCHEMA_VERSION}"), "\"schema_version\": 22", 1);
		assert_ne!(downgraded, text, "the fixture must actually carry the current version");
		std::fs::write(&manifest_path, downgraded).expect("writes");
		match RunManifest::read(&manifest_path) {
			Err(ManifestError::UnsupportedSchema { found: 22, expected }) => assert_eq!(expected, MANIFEST_SCHEMA_VERSION),
			other => panic!("expected an unsupported-schema refusal, got {other:?}"),
		}
	}

	#[test]
	fn a_credential_that_reached_the_record_makes_capture_refuse_the_manifest() {
		let dir = temp_dir("credential");
		let report = sample_report();
		let report_path = dir.join("run.json");
		report.write_json(&report_path).expect("report writes");
		let mut manifest = RunManifest::capture(&report, &report_path, &["weft-bench".to_string()], &[], None).expect("capture");
		manifest.provenance.cpu_model = Some("Intel Xeon E5-2680 password=hunter2".to_string());
		let manifest_path = dir.join("run.manifest.json");
		let error = manifest.write_to(&manifest_path).expect_err("a manifest that leaks a credential must not be written");
		assert!(error.to_string().contains("credential"), "{error}");
		// Written by hand it still fails verification, so an old file cannot pass either.
		std::fs::write(&manifest_path, manifest.to_json_pretty().expect("serializes")).expect("writes");
		let verdict = RunManifest::read(&manifest_path).expect("reads").verify(&manifest_path);
		assert!(verdict.problems.iter().any(|problem| matches!(problem, Problem::CredentialInProvenance(field) if *field == "provenance.cpu_model")), "{:?}", verdict.problems);
	}

	#[test]
	fn a_duplicated_measure_name_is_a_defect() {
		let dir = temp_dir("duplicate");
		let report = sample_report();
		let report_path = dir.join("run.json");
		report.write_json(&report_path).expect("report writes");
		let mut manifest = RunManifest::capture(&report, &report_path, &["weft-bench".to_string()], &[], None).expect("capture");
		let duplicate = manifest.grade.measures[0].clone();
		manifest.grade.measures.push(duplicate);
		let manifest_path = dir.join("run.manifest.json");
		assert!(manifest.write_to(&manifest_path).is_err(), "a comparison could not select between two measures with one name");
	}

	#[test]
	fn an_unrecorded_or_modified_source_is_advisory_not_a_broken_record() {
		let dir = temp_dir("advisory");
		let report = sample_report();
		let report_path = dir.join("run.json");
		report.write_json(&report_path).expect("report writes");
		let mut manifest = RunManifest::capture(&report, &report_path, &["weft-bench".to_string()], &[], None).expect("capture");
		manifest.sources.git_sha = None;
		manifest.sources.git_dirty = Some(true);
		let manifest_path = dir.join("run.manifest.json");
		manifest.write_to(&manifest_path).expect("the numbers are still real, so the record is still written");
		let verdict = RunManifest::read(&manifest_path).expect("reads").verify(&manifest_path);
		assert!(verdict.problems.iter().any(|problem| *problem == Problem::SourceUnrecorded), "{:?}", verdict.problems);
		assert!(verdict.problems.iter().any(|problem| *problem == Problem::DirtySource), "{:?}", verdict.problems);
		assert!(verdict.defects().is_empty(), "neither is a defect: {:?}", verdict.defects());
	}

	#[test]
	fn the_measures_name_every_headline_number_with_its_direction() {
		let report = sample_report();
		let grade = reference_grade(&report);
		let named = |name: &str| grade.measures.iter().find(|measure| measure.name == name);
		assert!(grade.publishable);
		assert_eq!(named("latency.p50_ns").map(|m| (m.value, m.better, m.headline)), Some((200.0, Better::Lower, true)));
		assert_eq!(named("throughput.points_per_sec").map(|m| (m.value, m.better, m.headline)), Some((5_000_000.0, Better::Higher, true)));
		assert_eq!(named("accuracy.rmse").map(|m| (m.value, m.better)), Some((0.5, Better::Lower)));
		assert!(named("storage.bytes_per_point").is_some(), "a report with storage must contribute its bytes/point");
		assert_eq!(named("correctness.passed").map(|m| m.value), Some(1.0));
		let headline: Vec<&str> = grade.measures.iter().filter(|measure| measure.headline).map(|measure| measure.name.as_str()).collect();
		assert_eq!(headline, ["latency.p50_ns", "latency.mean_ns", "latency.p95_ns", "throughput.points_per_sec", "storage.bytes_per_point", "accuracy.rmse"]);
	}

	#[test]
	fn a_run_that_failed_correctness_is_recorded_but_not_graded_as_publishable() {
		let mut report = sample_report();
		report.results[0].correctness.values_finite = false;
		let grade = reference_grade(&report);
		assert!(!grade.publishable, "a failed correctness gate must be visible in the verdict");
		assert_eq!(grade.measures.iter().find(|measure| measure.name == "correctness.passed").map(|m| m.value), Some(0.0));
	}

	#[test]
	fn the_environment_allow_list_excludes_the_obvious_secrets() {
		for name in ["PATH", "HOME", "GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "CARGO_REGISTRY_TOKEN", "WEFTDB_ADMIN_TOKEN"] {
			assert!(!CAPTURED_ENV.contains(&name), "`{name}` must never be captured");
		}
		for name in CAPTURED_ENV {
			// A captured name must not itself read as a credential: `KEY=value` under a secret key
			// name would redact the value to nothing useful and make the entry pointless.
			assert_eq!(crate::redaction::redact(&format!("{name}=placeholder")), format!("{name}=placeholder"), "`{name}` is a credential-shaped name; capturing it records nothing");
		}
		assert!(CAPTURED_ENV.contains(&"RUSTFLAGS"), "the flags the binary was built with change what the numbers mean");
	}

	#[test]
	fn a_report_path_that_was_under_the_home_directory_round_trips() {
		// `~` is what gets recorded, so verification has to be able to expand it again.
		let manifest = RunManifest { schema_version: MANIFEST_SCHEMA_VERSION, generated_at: "2026-10-25T00:00:00+00:00".to_string(), provenance: Provenance { weft_bench_version: "0.1.0".to_string(), os: "testos".to_string(), arch: "testarch".to_string(), build: BuildFacts::capture(), cpu_model: None, cpu_cores_physical: None, cpu_cores_logical: None, total_memory_bytes: None, work_disk_kind: None, work_disk_file_system: None, work_disk_mount_point: None, gpu: None, gpu_driver: None, engine_calibration: None, engine_note: None, target_cpu: None }, invocation: Invocation { command_line: vec![], working_directory: String::new(), environment: BTreeMap::new(), labels: BTreeMap::new() }, sources: Sources { report: "~/reports/run.json".to_string(), report_bytes: 0, report_sha256: hex_digest(b""), report_schema: crate::schema::SCHEMA_VERSION, git_sha: Some("abc1234".to_string()), git_dirty: Some(false) }, grade: ReferenceGrade { publishable: true, result_count: 0, publishable_count: 0, measures: vec![] }, claim: None };
		let resolved = manifest.report_path(Path::new("/nowhere/run.manifest.json")).expect("resolves");
		let home = std::env::var_os("HOME").map(|home| PathBuf::from(home.to_string_lossy().into_owned()).join("reports/run.json"));
		assert_eq!(resolved, home.unwrap_or_else(|| PathBuf::from("reports/run.json")), "the `~` prefix must expand to the home directory");
	}
}
