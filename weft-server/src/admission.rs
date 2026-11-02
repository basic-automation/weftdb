//! Admission on the serve path (1.0 plan **ROB-10**, "Admission I").
//!
//! SEC-3 folded ROB-10's per-route class table into the sealed route
//! registry: every route carries an [`AdmissionClass`](crate::routes::AdmissionClass)
//! in its `RouteSpec`. D-S14 then put the first piece of admission
//! *inside* the registry's gate (plan C10): the ingest routes'
//! byte-weighted in-flight budget. This module is the rest of
//! Admission I — the gate now consults the matched route's
//! `admission_class` for **every** class, not only the write class's
//! byte budget:
//!
//! - **Probe** — never shed. A shed probe reads as a dead process and
//!   starts the restart machinery, which is the most expensive answer
//!   available (the class's own rationale in `routes.rs`).
//! - **Query** — reads and stateless compute, shed first under load:
//!   at most [`AdmissionConfig::max_query_inflight`] query-class
//!   requests run at once; one beyond the cap is refused
//!   `503 overloaded` with `Retry-After` (plan §4) instead of queueing
//!   without bound behind the queries already running.
//! - **Write** — point writes and declarations, charged byte-weighted
//!   against the ingest in-flight budget (D-S14's
//!   [`IngestAdmission`]): the charge
//!   is keyed on the route's *class*, so a declaration — the other
//!   write-class route — is charged the same way an ingest is.
//! - **Maintenance** — maintenance and backup: long, exclusive-flavoured
//!   work. One runs at a time through the gate; a second admission
//!   while one runs is refused `409 busy` with `Retry-After`, the
//!   answer the class table has always promised for this class (the
//!   store's own per-aspect maintenance locks still answer `409` for
//!   conflicts below the whole class).
//!
//! The knobs live in the VarSpec registry (plan §3, PAC-5a/PAC-6 row:
//! ROB-10's knob registry is absorbed by the VarSpec registry), not in
//! a ROB-10-owned registry: the write budget is D-S14's
//! `WEFT_INGEST_MAX_INFLIGHT_BYTES`, and the query cap is
//! `WEFT_QUERY_MAX_INFLIGHT`. Maintenance exclusivity is the class's
//! definition, not a knob, so it takes no variable. Admission II
//! (ROB-11, W6) owns whatever refinement comes next (it arrives with
//! PAC-7's runtime builder, plan §2 L-MAIN); nothing here pre-empts it.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::routes::IngestAdmission;

/// Environment variable naming the query class's in-flight cap
/// (ROB-10): a positive whole number of requests, read once at
/// startup. Registered in the VarSpec registry ([`crate::settings`]).
pub const QUERY_MAX_INFLIGHT_ENV: &str = "WEFT_QUERY_MAX_INFLIGHT";

/// The default query-class in-flight cap when `WEFT_QUERY_MAX_INFLIGHT`
/// is unset: 128 concurrent reads/computes — far under the serve
/// path's connection cap (SEC-4), so a query flood is shed by class
/// long before it can starve the listener, while a single client
/// paging a store never comes near it.
pub const DEFAULT_QUERY_MAX_INFLIGHT: usize = 128;

/// Admission I's limits (ROB-10), read once at startup and carried on
/// the router state, so the registry the binary serves and a test's
/// registry can carry different caps. The write class's byte budget
/// is not here: it is D-S14's [`IngestConfig`](crate::ingest::IngestConfig),
/// and the two configs travel side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionConfig {
	max_query_inflight: usize,
}

impl AdmissionConfig {
	/// The defaults: [`DEFAULT_QUERY_MAX_INFLIGHT`] query-class
	/// requests in flight at once.
	pub const DEFAULT: Self = Self { max_query_inflight: DEFAULT_QUERY_MAX_INFLIGHT };

	/// A configuration with an explicit query-class cap (tests,
	/// embedders).
	#[must_use]
	pub const fn with_max_query_inflight(max_query_inflight: usize) -> Self {
		Self { max_query_inflight }
	}

	/// The limits the `WEFT_QUERY_MAX_INFLIGHT` value describes
	/// (`None` for an unset variable, which leaves the default).
	///
	/// # Errors
	///
	/// A message naming the variable when the value is not a positive
	/// whole number, so a malformed setting fails startup instead of
	/// silently falling back to a default the operator did not ask for.
	pub fn from_env_values(max_query_inflight: Option<&str>) -> Result<Self, String> {
		let mut config = Self::DEFAULT;
		if let Some(raw) = max_query_inflight {
			config.max_query_inflight = raw.parse().map_err(|e| format!("{QUERY_MAX_INFLIGHT_ENV}={raw:?} is not a positive integer number of requests: {e}"))?;
			if config.max_query_inflight == 0 {
				return Err(format!("{QUERY_MAX_INFLIGHT_ENV}={raw:?} is not a positive integer number of requests: zero would refuse every query"));
			}
		}
		Ok(config)
	}

	/// The limits the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// A set variable whose value is not a positive whole number.
	pub fn from_env() -> Result<Self, String> {
		Self::from_env_values(std::env::var(QUERY_MAX_INFLIGHT_ENV).ok().as_deref())
	}

	/// The query class's in-flight cap, in requests.
	#[must_use]
	pub const fn max_query_inflight(self) -> usize {
		self.max_query_inflight
	}
}

impl Default for AdmissionConfig {
	fn default() -> Self {
		Self::DEFAULT
	}
}

/// The serve path's admission state (ROB-10): one budget per
/// [`AdmissionClass`](crate::routes::AdmissionClass) that sheds. The
/// registry's gate holds one of these and charges each matched request
/// against its class's budget before the handler runs; the permit is
/// held by the gate's future across the handler call and released
/// when the handler has answered.
pub struct Admission {
	ingest: IngestAdmission,
	query: Arc<Semaphore>,
	query_limit: usize,
	maintenance: Arc<Semaphore>,
}

impl Admission {
	/// Admission budgets from the two configs that carry them: the
	/// ingest wiring's in-flight byte budget for the write class
	/// (D-S14) and [`AdmissionConfig`] for the query class (ROB-10).
	/// The maintenance class is exclusive by definition — one permit.
	#[must_use]
	pub fn new(ingest: IngestAdmission, config: AdmissionConfig) -> Self {
		Self { ingest, query: Arc::new(Semaphore::new(config.max_query_inflight)), query_limit: config.max_query_inflight, maintenance: Arc::new(Semaphore::new(1)) }
	}

	/// The write class's byte-weighted budget (D-S14).
	#[must_use]
	pub const fn ingest(&self) -> &IngestAdmission {
		&self.ingest
	}

	/// The query class's in-flight cap, in requests.
	#[must_use]
	pub const fn query_limit(&self) -> usize {
		self.query_limit
	}

	/// Admit one query-class request, or refuse: a request beyond the
	/// cap gets no permit, and the gate answers `503 overloaded`
	/// instead of queueing it behind the queries already running.
	#[must_use]
	pub fn try_admit_query(&self) -> Option<OwnedSemaphorePermit> {
		self.query.clone().try_acquire_owned().ok()
	}

	/// Query-class requests currently in flight.
	#[must_use]
	pub fn query_in_flight(&self) -> usize {
		self.query_limit.saturating_sub(self.query.available_permits())
	}

	/// Admit one maintenance-class request, or refuse: the class is
	/// exclusive, so a second admission while one runs gets no permit
	/// and the gate answers `409 busy`.
	#[must_use]
	pub fn try_admit_maintenance(&self) -> Option<OwnedSemaphorePermit> {
		self.maintenance.clone().try_acquire_owned().ok()
	}

	/// Whether a maintenance-class request is currently in flight.
	#[must_use]
	pub fn maintenance_in_flight(&self) -> bool {
		self.maintenance.available_permits() == 0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn admission_config_parses_and_refuses_zero() {
		let config = AdmissionConfig::from_env_values(Some("7")).expect("valid");
		assert_eq!(config.max_query_inflight(), 7);
		assert_eq!(AdmissionConfig::from_env_values(None).expect("defaults"), AdmissionConfig::DEFAULT);
		assert_eq!(AdmissionConfig::DEFAULT.max_query_inflight(), DEFAULT_QUERY_MAX_INFLIGHT);
		assert!(AdmissionConfig::from_env_values(Some("0")).is_err());
		assert!(AdmissionConfig::from_env_values(Some("many")).is_err());
	}

	#[test]
	fn query_admission_caps_concurrency_and_releases_on_drop() {
		let admission = Admission::new(IngestAdmission::new(1024), AdmissionConfig::with_max_query_inflight(2));
		let first = admission.try_admit_query().expect("under the cap");
		let _second = admission.try_admit_query().expect("at the cap");
		assert_eq!(admission.query_in_flight(), 2);
		assert!(admission.try_admit_query().is_none(), "beyond the cap is refused, not queued");
		drop(first);
		assert_eq!(admission.query_in_flight(), 1);
		assert!(admission.try_admit_query().is_some(), "a released permit admits again");
	}

	#[test]
	fn maintenance_admission_is_exclusive() {
		let admission = Admission::new(IngestAdmission::new(1024), AdmissionConfig::DEFAULT);
		let running = admission.try_admit_maintenance().expect("the first maintenance admission");
		assert!(admission.maintenance_in_flight());
		assert!(admission.try_admit_maintenance().is_none(), "a second maintenance admission is refused while one runs");
		drop(running);
		assert!(!admission.maintenance_in_flight());
		assert!(admission.try_admit_maintenance().is_some());
	}
}
