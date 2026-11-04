//! Soak harness skeleton (1.0 plan SOA-1, W1) plus preflight, device
//! resolution and redacted soak outputs (1.0 plan SOA-2, W2).
//!
//! This is the foundation SOA-3 and SOA-4 build on, and deliberately
//! nothing more: a configuration, a lifecycle, a run record, and — since
//! SOA-2 — a preflight gate that runs before a harness starts. It does not
//! run a workload, drive a server, or schedule anything — those are SOA-4's
//! workload model and SOA-3's scheduler and null server, and they plug into
//! the seams defined here rather than redefining them.
//!
//! Two ownership rules from the plan (§3) shape this module, and later slices
//! must keep them:
//!
//! * **Run metadata is EVI-3's, not the soak's.** A [`SoakRun`] carries
//!   [`crate::report::RunMetadata`] — the same environment capture the bench
//!   reports use — and adds only the soak's own fields beside it. There is no
//!   second disk field, driver field, or environment probe in this module.
//!   SOA-2's device resolution reuses EVI-3's disk probe
//!   for the soak store directory; the resolved
//!   kind, file system and mount point are the same values
//!   [`RunMetadata`] records as `work_disk_*` for the working directory, and
//!   the resolution lives only in the preflight report — never as a field on
//!   [`SoakRun`].
//! * **Redaction is EVI-3's, not the soak's.** Any free-text string a soak run
//!   records goes through [`crate::redaction::redact`] at the single place it
//!   leaves the harness ([`SoakRun::redacted_label`], and the redacted
//!   serializations [`SoakRun::redacted_json`] and
//!   [`SoakPreflight::redacted_json`]); this module defines no secret list,
//!   token shape, or redaction routine of its own.
//! * **The soak schema version is the soak's own line.** [`SOAK_SCHEMA_VERSION`]
//!   is independent of the bench result schema
//!   ([`crate::schema::SCHEMA_VERSION`]) and the run manifest schema
//!   ([`crate::manifest::MANIFEST_SCHEMA_VERSION`]): it versions the soak run
//!   record only, so a soak-record change never renumbers bench artifacts and
//!   vice versa. SOA-2 adds no field to [`SoakRun`] or [`SoakConfig`], so
//!   the version stays at v1 until SOA-6 records the oracle verdict on the
//!   run record and takes v2; SOA-7 then records window digests
//!   ([`crate::soak_digest`]) on the record and takes v3; SOA-8 records
//!   the failure deck's phases, recovery windows and SIGTERM verdict
//!   ([`crate::soak_failure`]) on the record and takes v4; SOA-10 records
//!   the derived-read verdict ([`crate::soak_read`]) on the record and
//!   takes v5.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
	redaction, report::{DiskInfo, RunMetadata}, soak_digest::WindowDigest, soak_failure::{PhaseRecord, RecoveryRecord, SigtermVerdict, SoakPhase}, soak_oracle::OracleVerdict, soak_read::DerivedReadVerdict
};

/// Version of the soak run record. Bump on any breaking field change to
/// [`SoakRun`] or [`SoakConfig`].
///
/// v1 is the SOA-1 skeleton: a config, a lifecycle state, and a run record
/// carrying EVI-3's [`RunMetadata`] beside the soak's own fields. SOA-2 adds
/// preflight and redacted output beside the record, not fields on it, so v1
/// stands. v2 (SOA-6) adds the optional oracle verdict field on [`SoakRun`]
/// — a field on the record, so the version moves, on this line only.
/// v3 (SOA-7) adds the window digest list on [`SoakRun`], the same way.
/// v4 (SOA-8) adds the failure deck's phase log, recovery records and
/// SIGTERM verdict on [`SoakRun`], the same way.
/// v5 (SOA-10) adds the optional derived-read verdict on [`SoakRun`],
/// the same way.
pub const SOAK_SCHEMA_VERSION: u32 = 5;

/// Configuration for one soak run.
///
/// SOA-2 validates a config in preflight before a harness starts (it
/// re-checks these invariants, because the fields are public and can change
/// after construction); SOA-4 fills the workload in from here. Neither slice
/// redefines what a soak run is configured by.
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
///
/// `PartialEq` but not `Eq` since SOA-6: the oracle verdict a record can
/// carry holds [`GridValue`](crate::soak_workload::GridValue)s, whose float
/// variant has no total equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
	/// The soak oracle's verdict on this run (SOA-6), recorded through
	/// [`SoakHarness::record_oracle_verdict`] once the run's read-back has
	/// been judged. `None` until then — and forever for a run nobody
	/// judged, which must never read as a passed verdict.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub oracle: Option<OracleVerdict>,
	/// The window digests recorded against this run (SOA-7), in window
	/// order: the expected digests a driver computed from the workload
	/// model as each window closed, through
	/// [`SoakHarness::record_window_digest`]. Empty until the first
	/// window is recorded — and forever for a run nobody digested, which
	/// must never read as windows that matched.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub windows: Vec<WindowDigest>,
	/// The failure deck's phase log for this run (SOA-8), in the order
	/// phases were entered: one card's five phases, then the next
	/// card's, recorded through [`SoakHarness::record_phase`]. Empty
	/// until the first phase is recorded — and forever for a run no
	/// deck drove, which must never read as a deck that ran.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub phases: Vec<PhaseRecord>,
	/// The failure deck's recovery records for this run (SOA-8), in
	/// card order, recorded through [`SoakHarness::record_recovery`].
	/// Empty until the first card completes.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub recoveries: Vec<RecoveryRecord>,
	/// The SIGTERM verdict on this run's server (SOA-8), recorded
	/// through [`SoakHarness::record_sigterm_verdict`] once the deck's
	/// SIGTERM card has been judged. `None` until then — and forever
	/// for a run with no SIGTERM card, which must never read as a
	/// graceful shutdown.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub sigterm: Option<SigtermVerdict>,
	/// The derived-read verdict on this run (SOA-10), recorded through
	/// [`SoakHarness::record_derived_verdict`] once the run's derived
	/// reads have been judged. `None` until then — and forever for a run
	/// nobody checked, which must never read as a passed verdict.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub derived: Option<DerivedReadVerdict>,
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

	/// A copy of this record with the free-text label replaced by its
	/// redacted form. The raw record keeps the label as configured (it is
	/// the operator's own text); only this copy is fit to serialize.
	#[must_use]
	pub fn redacted_copy(&self) -> Self {
		let mut copy = self.clone();
		copy.config.label = self.redacted_label();
		copy
	}

	/// Serialize this record as a soak output artifact: pretty JSON of the
	/// redacted copy, so no credential carried in the label — a key
	/// assignment, a token shape, URL user-info, a PEM block, or a home
	/// path — can reach a published soak output.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn redacted_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(&self.redacted_copy())?)
	}
}

/// The device a soak store directory resolves to.
///
/// Found by EVI-3's disk probe ([`DiskInfo`]). This is preflight evidence only: it is reported by
/// [`SoakPreflight`] and never added to [`SoakRun`], so a soak record still
/// carries exactly one set of disk facts — EVI-3's `work_disk_*` fields on
/// its [`RunMetadata`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedDevice {
	/// Kind of the disk (`"ssd"` or `"hdd"`), when the platform reports it.
	pub kind: Option<String>,
	/// File system of the disk (e.g. `btrfs`, `ext4`), when available.
	pub file_system: Option<String>,
	/// Mount point of the disk.
	pub mount_point: String,
	/// Free space on the disk in bytes, as reported at probe time. Reported,
	/// never gated: preflight sets no free-space threshold (a soak's
	/// footprint depends on SOA-4's workload, which does not exist yet),
	/// so an operator reads this number rather than a pass/fail inventing
	/// one.
	pub available_bytes: u64,
}

impl From<DiskInfo> for ResolvedDevice {
	fn from(disk: DiskInfo) -> Self {
		Self { kind: disk.kind, file_system: disk.file_system, mount_point: disk.mount_point, available_bytes: disk.available_bytes }
	}
}

/// One preflight check: a named gate and whether it passed.
///
/// The detail string is redacted before it reaches any output (details quote
/// paths, which can carry a home directory or a credential-like name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightCheck {
	/// Stable name of the gate (e.g. `store_dir_exists`).
	pub name: String,
	/// Whether the gate passed.
	pub passed: bool,
	/// Human-readable detail; redacted via [`PreflightCheck::redacted_detail`]
	/// before output.
	pub detail: String,
}

impl PreflightCheck {
	/// The detail with EVI-3's redaction applied.
	#[must_use]
	pub fn redacted_detail(&self) -> String {
		redaction::redact(&self.detail)
	}
}

/// The result of running preflight for one harness.
///
/// A report is always produced (even when gates fail) so the
/// caller can show exactly what failed; [`SoakHarness::start_checked`]
/// refuses to start from a report that did not pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoakPreflight {
	/// Name of the run this preflight was run for. [`SoakHarness::start_checked`]
	/// checks it against the harness's config, so a preflight run for one
	/// run can never start a different run.
	pub run_name: String,
	/// Store directory the preflight was run against.
	pub store_dir: PathBuf,
	/// Device the store directory resolved to, or `None` when no mount
	/// contains it (in which case the `device_resolved` check failed).
	pub device: Option<ResolvedDevice>,
	/// The gates, in evaluation order.
	pub checks: Vec<PreflightCheck>,
}

impl SoakPreflight {
	/// Whether every gate passed and a device was resolved.
	#[must_use]
	pub fn passed(&self) -> bool {
		self.device.is_some() && self.checks.iter().all(|check| check.passed)
	}

	/// Names of the gates that failed, in evaluation order.
	#[must_use]
	pub fn failed_checks(&self) -> Vec<&str> {
		self.checks.iter().filter(|check| !check.passed).map(|check| check.name.as_str()).collect()
	}

	/// Serialize this report as a soak output artifact: pretty JSON with
	/// the store path and every check detail passed through EVI-3's
	/// redaction, so a path under the operator's home directory or a
	/// credential-like directory name cannot leak into a published output.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn redacted_json(&self) -> anyhow::Result<String> {
		let mut value = serde_json::to_value(self)?;
		if let Some(dir) = value.get("store_dir").and_then(serde_json::Value::as_str).map(str::to_string) {
			value["store_dir"] = serde_json::Value::String(redaction::redact(&dir));
		}
		if let Some(checks) = value.get_mut("checks").and_then(serde_json::Value::as_array_mut) {
			for check in checks {
				if let Some(detail) = check.get("detail").and_then(serde_json::Value::as_str).map(str::to_string) {
					check["detail"] = serde_json::Value::String(redaction::redact(&detail));
				}
			}
		}
		Ok(serde_json::to_string_pretty(&value)?)
	}
}

/// A soak harness: a config, its lifecycle, and its run record.
///
/// The skeleton drives no work itself. SOA-2 runs preflight
/// ([`Self::preflight`]) before [`Self::start_checked`]; SOA-3's scheduler
/// calls [`Self::record_step`] and then [`Self::finish`] or [`Self::abort`];
/// SOA-4 supplies the work each step performs. All three read the run back
/// through [`Self::run_record`]. [`Self::start`] remains as the ungated
/// SOA-1 transition; the scheduler path must use `start_checked` so a run
/// never starts on a store its preflight rejected.
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
		Self { run: SoakRun { soak_schema_version: SOAK_SCHEMA_VERSION, config, metadata, state: SoakState::Created, steps: 0, oracle: None, windows: Vec::new(), phases: Vec::new(), recoveries: Vec::new(), sigterm: None, derived: None } }
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

	/// Run preflight for this harness against `store_dir`: validate the
	/// config and resolve the store directory to a device, without starting
	/// anything and without writing anything.
	///
	/// The gates, in order, are:
	/// * `config_valid` — the config name is non-blank and the duration is
	///   positive (re-checked here because the config fields are public
	///   and can change after construction);
	/// * `store_dir_exists` — the store directory exists and is a directory
	///   (preflight never creates it: creating a store is the operator's
	///   or a later slice's decision, not a silent side effect of a check);
	/// * `device_resolved` — EVI-3's disk probe identifies the disk holding
	///   the directory, so the run's device is known before it starts;
	/// * `store_dir_writable` — the directory is not read-only.
	///
	/// A failed gate does not make this call fail: the returned report
	/// carries the failure, and [`Self::start_checked`] is what refuses to
	/// start from it. Only running preflight on a harness that is no
	/// longer `Created` is an error.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Created`.
	pub fn preflight(&self, store_dir: &Path) -> anyhow::Result<SoakPreflight> {
		anyhow::ensure!(self.run.state == SoakState::Created, "soak preflight can only run from Created, not {:?}", self.run.state);
		let config_valid = !self.run.config.name.trim().is_empty() && self.run.config.duration_secs > 0;
		let exists = store_dir.is_dir();
		let writable = exists && !std::fs::metadata(store_dir).is_ok_and(|metadata| metadata.permissions().readonly());
		let disk = crate::report::disk_for(store_dir);
		let device = disk.map(ResolvedDevice::from);
		let checks = vec![PreflightCheck { name: "config_valid".to_string(), passed: config_valid, detail: if config_valid { "config name non-blank and duration positive".to_string() } else { "config name is blank or duration is zero".to_string() } }, PreflightCheck { name: "store_dir_exists".to_string(), passed: exists, detail: format!("store directory {}", store_dir.display()) }, PreflightCheck { name: "device_resolved".to_string(), passed: device.is_some(), detail: device.as_ref().map_or_else(|| format!("no device found for {}", store_dir.display()), |device| format!("{} on {}", device.mount_point, device.file_system.as_deref().unwrap_or("unknown filesystem"))) }, PreflightCheck { name: "store_dir_writable".to_string(), passed: writable, detail: if writable { "store directory is writable".to_string() } else { "store directory is missing or read-only".to_string() } }];
		Ok(SoakPreflight { run_name: self.run.config.name.clone(), store_dir: store_dir.to_path_buf(), device, checks })
	}

	/// Move a `Created` harness to `Running`, but only on a preflight
	/// report that passed and was run for this harness's config name.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Created`, when the
	/// preflight did not pass (naming the failed gates), or when the
	/// preflight was run for a different run name.
	pub fn start_checked(&mut self, preflight: &SoakPreflight) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Created, "soak can only start from Created, not {:?}", self.run.state);
		anyhow::ensure!(preflight.run_name == self.run.config.name, "soak preflight was run for '{}', not '{}'", preflight.run_name, self.run.config.name);
		anyhow::ensure!(preflight.passed(), "soak preflight failed: {}", preflight.failed_checks().join(", "));
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

	/// Record the soak oracle's verdict on this run (SOA-6): the judgement
	/// of the run's read-back against the workload model, kept in the run
	/// record's metadata so a published run carries its verdict with it.
	///
	/// Recorded exactly once, while `Running` — the driver judges the
	/// read-back, records the verdict, and then finishes the run. A second
	/// recording is refused rather than overwriting the first: a verdict
	/// that can be quietly replaced is not evidence.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`, or when a
	/// verdict has already been recorded.
	pub fn record_oracle_verdict(&mut self, verdict: OracleVerdict) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak oracle verdict can only be recorded while Running, not {:?}", self.run.state);
		anyhow::ensure!(self.run.oracle.is_none(), "soak oracle verdict has already been recorded for this run");
		self.run.oracle = Some(verdict);
		Ok(())
	}

	/// Record the derived-read verdict against this run (SOA-10): the
	/// judgement of the run's derived reads (the stored downsample's
	/// buckets, derived by [`DerivedReadChecker`](crate::soak_read::DerivedReadChecker)
	/// from the workload model), kept in the run record beside the
	/// oracle verdict so a published run carries the evidence for its
	/// derived surface as well as its raw read-back.
	///
	/// Recorded exactly once, while `Running` — the driver judges the
	/// derived read, records the verdict, and then finishes the run. A
	/// second recording is refused rather than overwriting the first: a
	/// verdict that can be quietly replaced is not evidence.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`, or when a
	/// verdict has already been recorded.
	pub fn record_derived_verdict(&mut self, verdict: DerivedReadVerdict) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak derived-read verdict can only be recorded while Running, not {:?}", self.run.state);
		anyhow::ensure!(self.run.derived.is_none(), "soak derived-read verdict has already been recorded for this run");
		self.run.derived = Some(verdict);
		Ok(())
	}

	/// Record one window digest against this run (SOA-7): the expected
	/// digest of a window that just closed, kept in the run record so a
	/// published run carries the per-window evidence its verdict was
	/// reached over.
	///
	/// Recorded only while `Running`, and only in window order: a new
	/// window must start at or after the end of the last recorded one.
	/// Overlapping or out-of-order windows are refused rather than
	/// appended — a digest recorded twice for the same ticks would
	/// double-count its evidence, and a run's windows are its timeline.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`, or when the
	/// window overlaps or precedes the last recorded window.
	pub fn record_window_digest(&mut self, digest: WindowDigest) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak window digests can only be recorded while Running, not {:?}", self.run.state);
		if let Some(last) = self.run.windows.last() {
			anyhow::ensure!(digest.start_tick >= last.end_tick(), "soak window digest at tick {} overlaps or precedes the recorded window ending at tick {}", digest.start_tick, last.end_tick());
		}
		self.run.windows.push(digest);
		Ok(())
	}

	/// Record one failure-deck phase against this run (SOA-8): a line
	/// of the deck's phase log, kept in the run record so a published
	/// run carries the timeline its failures moved through.
	///
	/// Recorded only while `Running`, and only in phase order: the
	/// first record is a card's `Baseline`; each next record is the
	/// same card's next phase, or — after a card's `Complete` — a new
	/// card's `Baseline`, at a tick no earlier than the last record's.
	/// Out-of-order phases are refused rather than appended: a phase
	/// log that can be written in any order is not a timeline.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`, or when the
	/// phase does not follow the last recorded one.
	pub fn record_phase(&mut self, record: PhaseRecord) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak phases can only be recorded while Running, not {:?}", self.run.state);
		if let Some(last) = self.run.phases.last() {
			anyhow::ensure!(record.at_tick >= last.at_tick, "soak phase at tick {} precedes the recorded phase at tick {}", record.at_tick, last.at_tick);
			if last.phase == SoakPhase::Complete {
				anyhow::ensure!(record.phase == SoakPhase::Baseline && record.card != last.card, "after card '{}' completes, the next phase must be a new card's Baseline", last.card);
			} else {
				anyhow::ensure!(record.card == last.card && Some(record.phase) == last.phase.next(), "phase {:?} for card '{}' does not follow {:?} for card '{}'", record.phase, record.card, last.phase, last.card);
			}
		} else {
			anyhow::ensure!(record.phase == SoakPhase::Baseline, "the first recorded phase must be a card's Baseline, not {:?}", record.phase);
		}
		self.run.phases.push(record);
		Ok(())
	}

	/// Record one failure card's recovery against this run (SOA-8):
	/// its window, when recovery was observed, and whether it landed
	/// inside the window, kept in the run record beside the phase log.
	///
	/// Recorded only while `Running`, at most once per card — a
	/// second record for the same card is refused rather than
	/// overwriting the first, as with the oracle verdict.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`, or when a
	/// recovery for the same card has already been recorded.
	pub fn record_recovery(&mut self, record: RecoveryRecord) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak recoveries can only be recorded while Running, not {:?}", self.run.state);
		anyhow::ensure!(self.run.recoveries.iter().all(|existing| existing.card != record.card), "a recovery for card '{}' has already been recorded for this run", record.card);
		self.run.recoveries.push(record);
		Ok(())
	}

	/// Record the SIGTERM verdict on this run (SOA-8): the judgement
	/// of the server's graceful shutdown, kept in the run record so a
	/// published run carries the verdict with it.
	///
	/// Recorded exactly once, while `Running`, as with the oracle
	/// verdict: a verdict that can be quietly replaced is not
	/// evidence.
	///
	/// # Errors
	///
	/// Returns an error when the harness is not `Running`, or when a
	/// verdict has already been recorded.
	pub fn record_sigterm_verdict(&mut self, verdict: SigtermVerdict) -> anyhow::Result<()> {
		anyhow::ensure!(self.run.state == SoakState::Running, "soak SIGTERM verdict can only be recorded while Running, not {:?}", self.run.state);
		anyhow::ensure!(self.run.sigterm.is_none(), "soak SIGTERM verdict has already been recorded for this run");
		self.run.sigterm = Some(verdict);
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
		assert_eq!(record.soak_schema_version, 5, "the soak's own schema line: v1 from SOA-1, v2 from SOA-6's oracle field, v3 from SOA-7's window digests, v4 from SOA-8's failure-deck fields, v5 from SOA-10's derived-read field, independent of the bench schema");
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

	#[test]
	fn preflight_resolves_the_device_for_an_existing_directory() {
		let harness = SoakHarness::new(config(), metadata());
		let dir = std::env::temp_dir();
		let report = harness.preflight(&dir).expect("preflight from Created");
		assert!(report.passed(), "preflight on an existing directory must pass: {:?}", report.checks);
		let device = report.device.as_ref().expect("device resolved");
		assert!(!device.mount_point.is_empty(), "resolved device must carry a mount point");
		// Reuse, not a second disk field: for the working directory, the
		// resolved device is exactly EVI-3's work_disk_* capture.
		let current = std::env::current_dir().expect("current directory");
		let current_report = harness.preflight(&current).expect("preflight on the working directory");
		let current_device = current_report.device.as_ref().expect("device for the working directory");
		let record = harness.run_record();
		assert_eq!(current_device.kind, record.metadata.work_disk_kind, "resolved kind must be EVI-3's work_disk_kind");
		assert_eq!(current_device.file_system, record.metadata.work_disk_file_system, "resolved filesystem must be EVI-3's work_disk_file_system");
		assert_eq!(Some(current_device.mount_point.clone()), record.metadata.work_disk_mount_point, "resolved mount must be EVI-3's work_disk_mount_point");
	}

	#[test]
	fn preflight_reports_failure_without_starting_and_checked_start_refuses_it() {
		let mut harness = SoakHarness::new(config(), metadata());
		let missing = std::env::temp_dir().join("weftdb-soa-2-no-such-store-dir");
		assert!(!missing.exists(), "test precondition: the missing directory must not exist");
		let report = harness.preflight(&missing).expect("a failed preflight still produces a report");
		assert!(!report.passed(), "preflight on a missing directory must not pass");
		assert!(report.device.is_some(), "the disk under a missing directory still resolves; existence is a separate gate");
		assert!(report.failed_checks().contains(&"store_dir_exists"), "the failing gate must be named: {:?}", report.checks);
		assert!(report.failed_checks().contains(&"store_dir_writable"), "a missing directory is not writable: {:?}", report.checks);
		assert!(harness.start_checked(&report).is_err(), "checked start must refuse a failed preflight");
		assert_eq!(harness.state(), SoakState::Created, "a refused start must leave the harness Created");
	}

	#[test]
	fn preflight_runs_only_from_created_and_checked_start_binds_to_its_run() {
		let mut harness = SoakHarness::new(config(), metadata());
		let dir = std::env::temp_dir();
		let report = harness.preflight(&dir).expect("preflight from Created");
		harness.start_checked(&report).expect("checked start on a passing preflight");
		assert!(harness.preflight(&dir).is_err(), "preflight after start must fail");

		let other = SoakHarness::new(SoakConfig::new("other".to_string(), String::new(), 1, 60).expect("valid config"), metadata());
		let other_report = other.preflight(&dir).expect("preflight for the other run");
		let mut fresh = SoakHarness::new(config(), metadata());
		assert!(fresh.start_checked(&other_report).is_err(), "a preflight run for another run must not start this harness");
		assert_eq!(fresh.state(), SoakState::Created);
	}

	#[test]
	fn preflight_rechecks_a_config_mutated_after_construction() {
		let mut broken_config = config();
		broken_config.duration_secs = 0;
		let harness = SoakHarness::new(broken_config, metadata());
		let report = harness.preflight(&std::env::temp_dir()).expect("preflight report");
		assert!(!report.passed(), "a config mutated to a zero duration must fail preflight");
		assert!(report.failed_checks().contains(&"config_valid"), "the failing gate must be config_valid: {:?}", report.checks);
	}

	#[test]
	fn preflight_cannot_resolve_a_device_for_a_relative_path() {
		let harness = SoakHarness::new(config(), metadata());
		let report = harness.preflight(Path::new("weftdb-soa-2-relative-store")).expect("preflight report");
		assert!(report.device.is_none(), "a relative path matches no mount, so no device resolves");
		assert!(report.failed_checks().contains(&"device_resolved"), "the failing gate must be device_resolved: {:?}", report.checks);
	}

	#[test]
	fn derived_verdict_is_recorded_once_while_running() {
		use crate::soak_read::DerivedReadVerdict;

		let verdict = DerivedReadVerdict { ticks_judged: 2, buckets_checked: 40, buckets_present: 32, points_counted: 96, mismatch_count: 0, mismatches: Vec::new() };
		let mut harness = SoakHarness::new(config(), metadata());
		assert!(harness.run_record().derived.is_none(), "no verdict before one is recorded");
		assert!(harness.record_derived_verdict(verdict.clone()).is_err(), "a verdict cannot be recorded before the run starts");
		harness.start().expect("start");
		harness.record_derived_verdict(verdict.clone()).expect("record while Running");
		assert_eq!(harness.run_record().derived.as_ref(), Some(&verdict), "the recorded verdict is on the run record");
		assert!(harness.record_derived_verdict(verdict).is_err(), "a second verdict must not overwrite the first");
		harness.finish().expect("finish");
		assert!(harness.run_record().derived.is_some(), "the verdict survives the finish");
		// The field is additive: a v4 artifact without it deserializes
		// with no verdict.
		let mut json = serde_json::to_value(harness.run_record().redacted_copy()).expect("record to json");
		json.as_object_mut().expect("object").remove("derived");
		let back: SoakRun = serde_json::from_value(json).expect("v4-shaped record deserializes");
		assert!(back.derived.is_none(), "an absent derived field reads as no verdict, never a pass");
	}

	#[test]
	fn window_digests_are_recorded_in_window_order_while_running() {
		use crate::{soak_digest::expected_window_digest, soak_workload::WorkloadModel};

		let model = WorkloadModel::from_config(&config());
		let digest = |start: u64| expected_window_digest(&model, start, 4);
		let mut harness = SoakHarness::new(config(), metadata());
		assert!(harness.record_window_digest(digest(0)).is_err(), "a digest cannot be recorded before the run starts");
		harness.start().expect("start from Created");
		harness.record_window_digest(digest(0)).expect("first window while Running");
		harness.record_window_digest(digest(4)).expect("the next window starts where the last ended");
		assert!(harness.record_window_digest(digest(4)).is_err(), "the same window twice would double-count its evidence");
		assert!(harness.record_window_digest(digest(2)).is_err(), "an overlapping window must be refused");
		harness.record_window_digest(digest(12)).expect("a gap between windows is allowed: undigested ticks are simply absent");
		assert_eq!(harness.run_record().windows.len(), 3);
		harness.finish().expect("finish from Running");
		assert!(harness.record_window_digest(digest(16)).is_err(), "no digests after the run ends");
		assert_eq!(harness.run_record().windows.len(), 3, "the recorded digests survive the finish transition");

		let json = harness.run_record().redacted_json().expect("redacted run JSON");
		assert!(json.contains("\"windows\""), "the recorded digests are in the run's output artifact: {json}");
		let back: SoakRun = serde_json::from_str(&json).expect("run artifact deserializes");
		assert_eq!(back.windows, harness.run_record().windows);
	}

	#[test]
	fn a_v2_run_artifact_without_windows_still_deserializes() {
		let harness = SoakHarness::new(config(), metadata());
		let mut value = serde_json::to_value(harness.run_record()).expect("serialize run");
		value.as_object_mut().expect("run is an object").remove("windows");
		value["soak_schema_version"] = serde_json::json!(2);
		let back: SoakRun = serde_json::from_value(value).expect("v2 artifact deserializes");
		assert!(back.windows.is_empty(), "a v2 artifact carries no window digests");
	}

	#[test]
	fn redacted_run_json_carries_no_secret_in_any_shape() {
		let label = "api_key=hunter2 profile https://weft_reader:S3cr3t@metrics.internal/db -----BEGIN OPENSSH PRIVATE KEY-----b3BlbnNzaA";
		let config = SoakConfig::new("smoke".to_string(), label.to_string(), 7, 60).expect("valid config");
		let harness = SoakHarness::new(config, metadata());
		let json = harness.run_record().redacted_json().expect("redacted run output");
		assert!(!json.contains("hunter2"), "redacted run output must not carry the key: {json}");
		assert!(!json.contains("S3cr3t"), "redacted run output must not carry the URL password: {json}");
		assert!(!json.contains("b3BlbnNzaA"), "redacted run output must not carry the PEM body: {json}");
		assert!(json.contains(redaction::REDACTED), "redacted run output must carry the marker: {json}");
		let back: SoakRun = serde_json::from_str(&json).expect("redacted output must still deserialize");
		assert_eq!(back.config.label, harness.run_record().redacted_label(), "the deserialized label must be the redacted one");
		assert_eq!(back.soak_schema_version, SOAK_SCHEMA_VERSION, "redacted output keeps the soak version");
	}

	#[test]
	fn failure_deck_records_are_recorded_in_order_while_running() {
		use crate::soak_failure::{FailureDeck, MaintenanceDriver, MaintenanceKind, PhaseRecord, SigtermObservation};

		let mut driver = MaintenanceDriver::new(FailureDeck::standard());
		let mut harness = SoakHarness::new(config(), metadata());
		assert!(harness.record_phase(PhaseRecord { card: "maintenance-contention".to_string(), phase: SoakPhase::Baseline, at_tick: 0 }).is_err(), "no phases before the run starts");
		harness.start().expect("start from Created");
		// Drive the whole standard deck, feeding every record the driver
		// produces into the harness as it is produced.
		while !driver.is_complete() {
			let card = driver.current_card().expect("card").clone();
			driver.advance(card.inject_tick).expect("baseline");
			driver.advance(card.inject_tick).expect("injection");
			if card.maintenance != MaintenanceKind::None {
				driver.run_maintenance().expect("maintenance during recovery");
			}
			driver.observe_recovery(Some(card.inject_tick + 1)).expect("observe recovery");
			driver.advance(card.inject_tick + 1).expect("recovery");
			if card.kind == crate::soak_failure::FailureKind::Sigterm {
				let verdict = driver.observe_sigterm(SigtermObservation { signal_tick: Some(card.inject_tick), exit_code: Some(0), drained_within_grace: true, previous_clean_shutdown: Some(true) }).expect("judge SIGTERM");
				harness.record_sigterm_verdict(verdict).expect("record SIGTERM verdict while Running");
				assert!(harness.record_sigterm_verdict(verdict).is_err(), "a second SIGTERM verdict must not overwrite the first");
			}
			driver.advance(card.inject_tick + 1).expect("verification");
		}
		for record in driver.phases() {
			harness.record_phase(record.clone()).expect("phases record in order");
		}
		for record in driver.recoveries() {
			harness.record_recovery(record.clone()).expect("recoveries record in card order");
		}
		assert_eq!(harness.run_record().phases.len(), 15, "three cards, five phases each");
		assert_eq!(harness.run_record().recoveries.len(), 3);
		assert!(harness.run_record().recoveries.iter().all(RecoveryRecord::recovered), "every card recovered inside its window with its maintenance run");
		assert!(harness.run_record().sigterm.expect("SIGTERM verdict recorded").passed());
		harness.finish().expect("finish from Running");
		let json = harness.run_record().redacted_json().expect("redacted run JSON");
		assert!(json.contains("\"phases\"") && json.contains("\"recoveries\"") && json.contains("\"sigterm\""), "the failure-deck records are in the run's output artifact: {json}");
		let back: SoakRun = serde_json::from_str(&json).expect("run artifact deserializes");
		assert_eq!(back.phases, harness.run_record().phases);
		assert_eq!(back.sigterm, harness.run_record().sigterm);
	}

	#[test]
	fn phase_records_refuse_out_of_order_phases() {
		use crate::soak_failure::PhaseRecord;

		let record = |card: &str, phase: SoakPhase, at_tick: u64| PhaseRecord { card: card.to_string(), phase, at_tick };
		let mut harness = SoakHarness::new(config(), metadata());
		harness.start().expect("start");
		assert!(harness.record_phase(record("a", SoakPhase::Recovery, 0)).is_err(), "the first phase must be a Baseline");
		harness.record_phase(record("a", SoakPhase::Baseline, 0)).expect("baseline");
		assert!(harness.record_phase(record("a", SoakPhase::Recovery, 1)).is_err(), "phases cannot be skipped");
		assert!(harness.record_phase(record("b", SoakPhase::Injection, 1)).is_err(), "cards cannot interleave");
		harness.record_phase(record("a", SoakPhase::Injection, 1)).expect("injection");
	}

	#[test]
	fn a_v3_run_artifact_without_failure_fields_still_deserializes() {
		let harness = SoakHarness::new(config(), metadata());
		let mut value = serde_json::to_value(harness.run_record()).expect("serialize run");
		let object = value.as_object_mut().expect("run is an object");
		object.remove("phases");
		object.remove("recoveries");
		object.remove("sigterm");
		value["soak_schema_version"] = serde_json::json!(3);
		let back: SoakRun = serde_json::from_value(value).expect("v3 artifact deserializes");
		assert!(back.phases.is_empty() && back.recoveries.is_empty() && back.sigterm.is_none(), "a v3 artifact carries no failure-deck records");
	}

	#[test]
	fn redacted_preflight_json_hides_paths_and_credential_like_names() {
		let harness = SoakHarness::new(config(), metadata());
		let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
		let secret_dir = home.join("api_key=hunter2-store");
		let report = harness.preflight(&secret_dir).expect("preflight report for a missing directory");
		let json = report.redacted_json().expect("redacted preflight output");
		assert!(!json.contains("hunter2"), "redacted preflight output must not carry the credential-like name: {json}");
		if let Some(home_str) = home.to_str() {
			assert!(!json.contains(home_str) || json.contains('~'), "redacted preflight output must not carry the raw home path: {json}");
		}
		assert!(json.contains(redaction::REDACTED) || json.contains('~'), "redacted preflight output must show a redaction: {json}");
	}
}
