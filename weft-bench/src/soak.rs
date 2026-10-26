//! Soak harness skeleton (1.0 plan SOA-1, W1).
//!
//! This is the foundation SOA-2, SOA-3 and SOA-4 build on, and deliberately
//! nothing more: a configuration, a lifecycle, and a run record. It does not
//! run a workload, drive a server, or schedule anything — those are SOA-4's
//! workload model, SOA-3's scheduler and null server, and SOA-2's preflight
//! respectively, and they plug into the seams defined here rather than
//! redefining them.
//!
//! Two ownership rules from the plan (§3) shape this module, and later slices
//! must keep them:
//!
//! * **Run metadata is EVI-3's, not the soak's.** A [`SoakRun`] carries
//!   [`crate::report::RunMetadata`] — the same environment capture the bench
//!   reports use — and adds only the soak's own fields beside it. There is no
//!   second disk field, driver field, or environment probe in this module.
//! * **Redaction is EVI-3's, not the soak's.** Any free-text string a soak run
//!   records goes through [`crate::redaction::redact`] at the single place it
//!   leaves the harness ([`SoakRun::redacted_label`]); this module defines no
//!   secret list, token shape, or redaction routine of its own.
//! * **The soak schema version is the soak's own line.** [`SOAK_SCHEMA_VERSION`]
//!   is independent of the bench result schema
//!   ([`crate::schema::SCHEMA_VERSION`]) and the run manifest schema
//!   ([`crate::manifest::MANIFEST_SCHEMA_VERSION`]): it versions the soak run
//!   record only, so a soak-record change never renumbers bench artifacts and
//!   vice versa.

use serde::{Deserialize, Serialize};

use crate::{redaction, report::RunMetadata};

/// Version of the soak run record. Bump on any breaking field change to
/// [`SoakRun`] or [`SoakConfig`].
///
/// v1 is the SOA-1 skeleton: a config, a lifecycle state, and a run record
/// carrying EVI-3's [`RunMetadata`] beside the soak's own fields.
pub const SOAK_SCHEMA_VERSION: u32 = 1;

/// Configuration for one soak run.
///
/// SOA-2 adds preflight and device resolution on top of this (it validates a
/// config before a harness starts); SOA-4 fills the workload in from here.
/// Neither slice redefines what a soak run is configured by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoakConfig {
	/// Human-readable name for the run, recorded in the run record.
	pub name: String,
	/// Free-text label for the run (profile name, operator note, ...). It is
	/// recorded only through [`SoakRun::redacted_label`], never raw, because
	/// free text can carry a credential.
	pub label: String,
	/// Seed for everything the run generates. SOA-4's workload model derives
	/// its deterministic workload from this seed alone.
	pub seed: u64,
	/// Planned run length in seconds. A skeleton run never enforces it — the
	/// SOA-3 scheduler does — but a zero length is not a run, so construction
	/// rejects it.
	pub duration_secs: u64,
}

impl SoakConfig {
	/// Build a config, rejecting a blank name or a zero duration.
	///
	/// # Errors
	///
	/// Returns an error when `name` is blank or `duration_secs` is zero.
	pub fn new(name: String, label: String, seed: u64, duration_secs: u64) -> anyhow::Result<Self> {
		anyhow::ensure!(!name.trim().is_empty(), "soak name must not be blank");
		anyhow::ensure!(duration_secs > 0, "soak duration_secs must be positive");
		Ok(Self { name, label, seed, duration_secs })
	}
}

/// Lifecycle state of a soak harness.
///
/// The transitions are [`SoakHarness::start`], [`SoakHarness::finish`] and
/// [`SoakHarness::abort`]; anything else is an error, so a later slice can
/// never silently drive a finished run or report a run that never started.
/// SOA-3's scheduler moves a harness through these states; SOA-8's phases are
/// a finer breakdown *within* `Running` and do not replace this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SoakState {
	/// Configured but not yet started.
	Created,
	/// Started; steps (SOA-3) and workload (SOA-4) run in this state.
	Running,
	/// Ended normally via [`SoakHarness::finish`].
	Finished,
	/// Ended early via [`SoakHarness::abort`].
	Aborted,
}

/// The record of one soak run: the soak's own version and config, the shared
/// EVI-3 run metadata, and the harness lifecycle.
///
/// Serialized as the soak run artifact. Older artifacts still deserialize:
/// every field added after v1 must be `#[serde(default)]` (and
/// `skip_serializing_if` when optional), as the bench result schema does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoakRun {
	/// Soak schema version this record was written with (mirrors
	/// [`SOAK_SCHEMA_VERSION`]).
	pub soak_schema_version: u32,
	/// The configuration this run was started from.
	pub config: SoakConfig,
	/// The shared EVI-3 run metadata for this run: crate version, OS, arch,
	/// hardware probe, and generation timestamp. Reused, not redefined.
	pub metadata: RunMetadata,
	/// Lifecycle state at the time the record was taken.
	pub state: SoakState,
	/// Number of steps recorded against this run (SOA-3's scheduler ticks).
	pub steps: u64,
}

impl SoakRun {
	/// The config's free-text label, passed through EVI-3's redaction.
	///
	/// This is the only path by which a soak label reaches an artifact, so a
	/// later slice that records labels must call this rather than reading
	/// `config.label` directly.
	#[must_use]
	pub fn redacted_label(&self) -> String {
		redaction::redact(&self.config.label)
	}
}

/// A soak harness: a config, its lifecycle, and its run record.
///
/// The skeleton drives no work itself. SOA-2 runs preflight before
/// [`Self::start`]; SOA-3's scheduler calls [`Self::record_step`] and then
/// [`Self::finish`] or [`Self::abort`]; SOA-4 supplies the work each step
/// performs. All three read the run back through [`Self::run_record`].
#[derive(Debug)]
pub struct SoakHarness {
	run: SoakRun,
}

impl SoakHarness {
	/// Configure a harness. The run record starts in [`SoakState::Created`]
	/// with zero steps; `metadata` is the caller's EVI-3 capture (see
	/// [`RunMetadata::capture`]), not a second probe.
	#[must_use]
	pub const fn new(config: SoakConfig, metadata: RunMetadata) -> Self {
		Self { run: SoakRun { soak_schema_version: SOAK_SCHEMA_VERSION, config, metadata, state: SoakState::Created, steps: 0 } }
	}

	/// Current lifecycle state.
	#[must_use]
	pub const fn state(&self) -> SoakState {
		self.run.state
	}

	/// Move a `Created` harness to `Running`.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Created` (already started,
	/// finished, or aborted).
	pub fn start(&mut self) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Created, "soak can only start from Created, not {:?}", self.run.state);
		self.run.state = SoakState::Running;
		Ok(())
	}

	/// Record one step against a `Running` harness (the SOA-3 scheduler's
	/// tick seam; the skeleton counts steps, it does not perform them).
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`.
	pub fn record_step(&mut self) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak steps can only be recorded while Running, not {:?}", self.run.state);
		self.run.steps = self.run.steps.saturating_add(1);
		Ok(())
	}

	/// Move a `Running` harness to `Finished`.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`.
	pub fn finish(&mut self) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak can only finish from Running, not {:?}", self.run.state);
		self.run.state = SoakState::Finished;
		Ok(())
	}

	/// Move a `Running` harness to `Aborted`.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`.
	pub fn abort(&mut self) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak can only abort from Running, not {:?}", self.run.state);
		self.run.state = SoakState::Aborted;
		Ok(())
	}

	/// The run record as it currently stands.
	#[must_use]
	pub const fn run_record(&self) -> &SoakRun {
		&self.run
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config() -> SoakConfig {
		SoakConfig::new("smoke".to_string(), "nightly profile".to_string(), 7, 60).expect("valid config")
	}

	fn metadata() -> RunMetadata {
		RunMetadata::capture("2026-10-25T00:00:00+00:00".to_string())
	}

	#[test]
	fn config_rejects_blank_name_and_zero_duration() {
		assert!(SoakConfig::new(String::new(), String::new(), 0, 60).is_err(), "blank name must be rejected");
		assert!(SoakConfig::new("   ".to_string(), String::new(), 0, 60).is_err(), "whitespace-only name must be rejected");
		assert!(SoakConfig::new("smoke".to_string(), String::new(), 0, 0).is_err(), "zero duration must be rejected");
		assert!(SoakConfig::new("smoke".to_string(), String::new(), 0, 1).is_ok(), "minimal valid config must be accepted");
	}

	#[test]
	fn lifecycle_allows_only_the_documented_transitions() {
		let mut harness = SoakHarness::new(config(), metadata());
		assert_eq!(harness.state(), SoakState::Created);
		assert!(harness.finish().is_err(), "finish before start must fail");
		assert!(harness.record_step().is_err(), "steps before start must fail");
		assert!(harness.abort().is_err(), "abort before start must fail");

		harness.start().expect("start from Created");
		assert_eq!(harness.state(), SoakState::Running);
		assert!(harness.start().is_err(), "second start must fail");
		harness.record_step().expect("step while Running");
		harness.record_step().expect("second step while Running");
		assert_eq!(harness.run_record().steps, 2);

		harness.finish().expect("finish from Running");
		assert_eq!(harness.state(), SoakState::Finished);
		assert!(harness.start().is_err(), "start after finish must fail");
		assert!(harness.record_step().is_err(), "steps after finish must fail");
		assert!(harness.finish().is_err(), "second finish must fail");
		assert!(harness.abort().is_err(), "abort after finish must fail");
	}

	#[test]
	fn abort_ends_a_running_harness_without_finishing_it() {
		let mut harness = SoakHarness::new(config(), metadata());
		harness.start().expect("start");
		harness.abort().expect("abort from Running");
		assert_eq!(harness.state(), SoakState::Aborted);
		assert_eq!(harness.run_record().steps, 0, "aborted run keeps the steps it recorded");
	}

	#[test]
	fn run_record_reuses_evi3_metadata_and_carries_the_soak_version() {
		let mut harness = SoakHarness::new(config(), metadata());
		harness.start().expect("start");
		let record = harness.run_record();
		assert_eq!(record.soak_schema_version, SOAK_SCHEMA_VERSION);
		assert_eq!(record.soak_schema_version, 1, "the soak's own schema line starts at 1, independent of the bench schema");
		// Reuse, not redefinition: the record's metadata is EVI-3's capture,
		// with the same crate version / OS / arch / timestamp it was built from.
		assert!(!record.metadata.weft_bench_version.is_empty(), "metadata must carry the bench crate version");
		assert_eq!(record.metadata.os, std::env::consts::OS);
		assert_eq!(record.metadata.arch, std::env::consts::ARCH);
		assert_eq!(record.metadata.generated_at, "2026-10-25T00:00:00+00:00");
		assert_eq!(record.config.seed, 7);
	}

	#[test]
	fn run_record_round_trips_through_json() {
		let mut harness = SoakHarness::new(config(), metadata());
		harness.start().expect("start");
		harness.record_step().expect("step");
		harness.finish().expect("finish");
		let json = serde_json::to_string(harness.run_record()).expect("serialize run record");
		let back: SoakRun = serde_json::from_str(&json).expect("deserialize run record");
		assert_eq!(*harness.run_record(), back, "run record must survive a JSON round-trip");
	}

	#[test]
	fn label_reaches_artifacts_only_through_evi3_redaction() {
		let config = SoakConfig::new("smoke".to_string(), "api_key=hunter2 profile".to_string(), 7, 60).expect("valid config");
		let harness = SoakHarness::new(config, metadata());
		let redacted = harness.run_record().redacted_label();
		assert!(!redacted.contains("hunter2"), "redacted label must not carry the credential: {redacted}");
		assert!(redacted.contains(redaction::REDACTED), "redacted label must carry the redaction marker: {redacted}");
		// Idempotence (EVI-3's guarantee) holds through the soak seam too.
		assert_eq!(redaction::redact(&redacted), redacted, "redacting twice must be a no-op");
	}
}
