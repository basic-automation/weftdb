//! Failure deck, phases, recovery windows, SIGTERM verdict and the
//! maintenance driver (1.0 plan SOA-8, W5).
//!
//! A soak that only ever runs against a healthy server proves the happy
//! path. This module is the harness side of proving the unhappy ones:
//! a **failure deck** — an ordered set of failure cards a run subjects
//! its server to — the **phases** each card moves through, the
//! **recovery window** each failure is allowed to recover inside, the
//! **SIGTERM verdict** on the server's graceful shutdown, and the
//! **maintenance driver** that drives the deck card by card.
//!
//! **Phases live within `Running`.** SOA-1's harness doc fixed this:
//! SOA-8's phases are a finer breakdown *within*
//! [`SoakState::Running`](crate::soak::SoakState::Running) and do not
//! replace the lifecycle state. A card moves
//! `Baseline → Injection → Recovery → Verification → Complete`, in
//! that order and no other; the driver refuses any other transition,
//! so a published phase log is a timeline that actually happened.
//!
//! **The deck.** A [`FailureDeck`] is validated on construction: card
//! ids are slugs (lowercase letters, digits and `-`, so a card id is
//! never free text and needs no redaction), ids are unique, every
//! recovery window is non-empty, cards are in injection order, and a
//! SIGTERM card — which ends the server — is at most one and is last.
//! The failure kinds are the ones the shipped server actually has:
//! [`FailureKind::MaintenanceContention`] (another maintenance
//! operation holds an aspect; the store's sweep skips it or answers
//! `busy`), [`FailureKind::ServerRestart`] (the process is stopped
//! and started again; the store reopens and reports its previous
//! shutdown), and [`FailureKind::Sigterm`] (the graceful-shutdown
//! path SEC-4/D-S14 built: drain inside the grace, exit 0, clean
//! shutdown recorded). SIGKILL is deliberately not a kind here: it is
//! SOA-9's `crash_kill9` (W6, with D-S21).
//!
//! **Recovery windows.** Each card names the window its recovery must
//! land in: ticks `inject_tick..inject_tick + recovery_ticks`. The
//! window is part of the deck the operator authored — this module
//! invents no recovery budget of its own — and recovery is judged
//! exactly, the SOA line's standing rule: a recovery observed inside
//! the window recovered; one observed at or after its end, or never
//! observed, did not. There is no tolerance and no rate.
//!
//! **The SIGTERM verdict.** [`SigtermVerdict`] is judged from a
//! [`SigtermObservation`] the caller supplies (the driver performs no
//! signal and no read itself, exactly as the oracle performs no read):
//! the signal was sent, the process exited, its exit code, whether it
//! drained within the grace the caller configured (the serve path's
//! `WEFT_SHUTDOWN_GRACE_SECS`, plan C9/C-2 — the grace is an input,
//! never a constant here), and whether the reopened store reported a
//! clean previous shutdown. [`SigtermVerdict::passed`] is the exact
//! conjunction; a signal that was never sent cannot pass vacuously.
//!
//! **The maintenance driver.** [`MaintenanceDriver`] drives the deck:
//! it holds the card order and the current phase, issues the current
//! [`DeckAction`] (run the baseline, inject the failure, run the
//! card's maintenance operation during recovery, await recovery,
//! verify), and collects what the caller observes — recovery ticks,
//! the maintenance run, and for the SIGTERM card the verdict — into
//! [`RecoveryRecord`]s, [`PhaseRecord`]s and the verdict. During a
//! card's recovery phase the driver runs the card's
//! [`MaintenanceKind`] (reconcile, squash, backup, or none): recovery
//! that only idles is not the recovery a real deployment performs,
//! where the daemons (ROB-3's registry) resume their sweeps. A card
//! whose maintenance is not `None` cannot leave recovery until that
//! maintenance has run.
//!
//! **Recorded in the run record.** Phase records, recovery records
//! and the SIGTERM verdict are recorded on the run they belong to
//! through the harness (`record_phase`, `record_recovery`,
//! `record_sigterm_verdict`), beside the oracle verdict (SOA-6) and
//! the window digests (SOA-7). Adding fields to
//! [`SoakRun`](crate::soak::SoakRun) takes
//! [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) to v4 on
//! SOA's own version line; the fields are `#[serde(default)]` and
//! skipped when empty/absent, so v1–v3 artifacts still deserialize.
//! None of the records carries free text beyond validated card-id
//! slugs, so they pass through redaction unchanged.

use serde::{Deserialize, Serialize};

/// The failure one card subjects the server to. See the module doc
/// for why these three, and why SIGKILL is not among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
	/// Another maintenance operation holds an aspect: sweeps skip it
	/// or answer `busy` until the holder releases it.
	MaintenanceContention,
	/// The server process is stopped and started again; the store
	/// reopens over the same directory.
	ServerRestart,
	/// SIGTERM: the graceful-shutdown path. Ends the server, so a
	/// deck carries at most one, last.
	Sigterm,
}

/// The maintenance operation the driver runs during a card's
/// recovery phase: the sweeps the server's daemons perform
/// (reconcile, squash, backup), or none for a card whose recovery
/// is observation only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceKind {
	/// No maintenance during this card's recovery.
	None,
	/// A reconcile sweep (the reconcile daemon's tick).
	Reconcile,
	/// A squash pass.
	Squash,
	/// A backup snapshot.
	Backup,
}

/// One phase of one card, in deck order. A card's phases run in
/// exactly this sequence; [`Self::next`] is the only successor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakPhase {
	/// Steady traffic before the failure, so recovery has a baseline
	/// to be judged against.
	Baseline,
	/// The failure is applied.
	Injection,
	/// The recovery window is open; the driver runs the card's
	/// maintenance in this phase.
	Recovery,
	/// The recovery (and, for the SIGTERM card, the verdict) is
	/// judged.
	Verification,
	/// The card is done; the driver moves to the next card.
	Complete,
}

impl SoakPhase {
	/// The phase after this one, or `None` at [`Self::Complete`].
	#[must_use]
	pub const fn next(self) -> Option<Self> {
		match self {
			Self::Baseline => Some(Self::Injection),
			Self::Injection => Some(Self::Recovery),
			Self::Recovery => Some(Self::Verification),
			Self::Verification => Some(Self::Complete),
			Self::Complete => None,
		}
	}

	/// Every phase, in order: the timeline one card produces.
	#[must_use]
	pub const fn all() -> [Self; 5] {
		[Self::Baseline, Self::Injection, Self::Recovery, Self::Verification, Self::Complete]
	}
}

/// One card in a failure deck: a failure, when it lands, the window
/// its recovery must land in, and the maintenance the driver runs
/// during that recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureCard {
	/// Slug naming the card (validated by [`FailureDeck::new`]).
	pub id: String,
	/// The failure this card applies.
	pub kind: FailureKind,
	/// Tick at which the failure is injected.
	pub inject_tick: u64,
	/// Ticks in the recovery window: recovery must be observed in
	/// `inject_tick..inject_tick + recovery_ticks`.
	pub recovery_ticks: u64,
	/// Maintenance the driver runs during recovery.
	pub maintenance: MaintenanceKind,
}

impl FailureCard {
	/// This card's recovery window.
	#[must_use]
	pub const fn recovery_window(&self) -> RecoveryWindow {
		RecoveryWindow { start_tick: self.inject_tick, tick_count: self.recovery_ticks }
	}
}

/// A recovery window: a contiguous tick range, the same shape as a
/// digest window (SOA-7) — `start_tick..start_tick + tick_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryWindow {
	/// First tick of the window (the injection tick).
	pub start_tick: u64,
	/// Ticks in the window.
	pub tick_count: u64,
}

impl RecoveryWindow {
	/// The first tick after this window.
	#[must_use]
	pub const fn end_tick(&self) -> u64 {
		self.start_tick.saturating_add(self.tick_count)
	}

	/// Whether `tick` is inside the window.
	#[must_use]
	pub const fn contains(&self, tick: u64) -> bool {
		tick >= self.start_tick && tick < self.end_tick()
	}

	/// Whether a recovery observed at `recovered_at` (or never, as
	/// `None`) landed inside the window. Exact: at the end tick is
	/// already too late.
	#[must_use]
	pub const fn recovered_within(&self, recovered_at: Option<u64>) -> bool {
		match recovered_at {
			Some(tick) => self.contains(tick),
			None => false,
		}
	}
}

/// Whether a card id is a slug: non-empty, lowercase letters, digits
/// and `-`, not starting or ending with `-`. Slugs keep card ids out
/// of free text, so no record carrying one needs redaction.
fn is_slug(id: &str) -> bool {
	!id.is_empty() && !id.starts_with('-') && !id.ends_with('-') && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A failure deck: the cards of one run, in the order the driver
/// deals them. Validated on construction; see the module doc for
/// the rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDeck {
	/// The cards, in injection order.
	pub cards: Vec<FailureCard>,
}

impl FailureDeck {
	/// Build a deck, rejecting an empty deck, a non-slug or duplicate
	/// card id, an empty recovery window, cards out of injection
	/// order, and a SIGTERM card that is not the only one and last.
	///
	/// # Errors
	///
	/// Returns an error naming the first broken rule.
	pub fn new(cards: Vec<FailureCard>) -> anyhow::Result<Self> {
		anyhow::ensure!(!cards.is_empty(), "a failure deck must hold at least one card");
		for (index, card) in cards.iter().enumerate() {
			anyhow::ensure!(is_slug(&card.id), "failure card id '{}' is not a slug", card.id);
			anyhow::ensure!(card.recovery_ticks > 0, "failure card '{}' has an empty recovery window", card.id);
			anyhow::ensure!(cards[..index].iter().all(|earlier| earlier.id != card.id), "failure card id '{}' appears twice", card.id);
			if index > 0 {
				anyhow::ensure!(card.inject_tick >= cards[index - 1].inject_tick, "failure cards must be in injection order: '{}' injects before the card before it", card.id);
			}
			if card.kind == FailureKind::Sigterm {
				anyhow::ensure!(index == cards.len() - 1, "the SIGTERM card '{}' ends the server and must be last", card.id);
			}
		}
		Ok(Self { cards })
	}

	/// The standard deck: contention, a restart, and SIGTERM last,
	/// each recovering under a different maintenance sweep. Ticks are
	/// the deck author's schedule for a run whose baseline is counted
	/// in scheduler ticks; a caller with another cadence builds its
	/// own deck with [`Self::new`].
	#[must_use]
	pub fn standard() -> Self {
		Self { cards: vec![FailureCard { id: "maintenance-contention".to_string(), kind: FailureKind::MaintenanceContention, inject_tick: 100, recovery_ticks: 50, maintenance: MaintenanceKind::Reconcile }, FailureCard { id: "server-restart".to_string(), kind: FailureKind::ServerRestart, inject_tick: 200, recovery_ticks: 50, maintenance: MaintenanceKind::Squash }, FailureCard { id: "sigterm".to_string(), kind: FailureKind::Sigterm, inject_tick: 300, recovery_ticks: 50, maintenance: MaintenanceKind::Backup }] }
	}

	/// The card at `index`, if any.
	#[must_use]
	pub fn card(&self, index: usize) -> Option<&FailureCard> {
		self.cards.get(index)
	}

	/// How many cards the deck holds.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.cards.len()
	}

	/// Whether the deck holds no cards (only reachable by
	/// deserializing a deck that bypassed [`Self::new`]).
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.cards.is_empty()
	}

	/// Serialize this deck as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// What a caller observed of one SIGTERM: the inputs to the verdict.
/// Supplied by the caller; the harness sends no signal itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigtermObservation {
	/// Tick at which SIGTERM was sent. `None` when it never was —
	/// which cannot produce a passing verdict.
	pub signal_tick: Option<u64>,
	/// Exit code the process exited with, or `None` when it had not
	/// exited when observation ended.
	pub exit_code: Option<i32>,
	/// Whether the process drained and exited within the configured
	/// shutdown grace (an input from the serve path's configuration,
	/// not a constant of this module).
	pub drained_within_grace: bool,
	/// Whether the store, reopened after the exit, reported a clean
	/// previous shutdown (`previous_clean_shutdown` on `/ready`).
	/// `None` when the store was never reopened.
	pub previous_clean_shutdown: Option<bool>,
}

/// The SIGTERM verdict: the observation, judged exactly. Recorded on
/// the run record once, via the harness. Carries no free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigtermVerdict {
	/// Tick at which SIGTERM was sent, when it was.
	pub signal_tick: Option<u64>,
	/// Exit code the process exited with, when it did.
	pub exit_code: Option<i32>,
	/// Whether it drained within the configured grace.
	pub drained_within_grace: bool,
	/// Whether the reopened store reported a clean previous
	/// shutdown, when it was reopened.
	pub previous_clean_shutdown: Option<bool>,
}

impl SigtermVerdict {
	/// Judge one observation: the verdict is the observation, kept
	/// whole so a reviewer sees every input beside the judgement.
	#[must_use]
	pub const fn judge(observation: SigtermObservation) -> Self {
		Self { signal_tick: observation.signal_tick, exit_code: observation.exit_code, drained_within_grace: observation.drained_within_grace, previous_clean_shutdown: observation.previous_clean_shutdown }
	}

	/// Whether the shutdown was graceful, exactly: the signal was
	/// sent, the process exited 0 (a signal before serving also
	/// exits 0, plan C9), it drained within the configured grace,
	/// and the reopened store reported a clean previous shutdown.
	/// Any missing observation fails — an unobserved shutdown is
	/// not a graceful one.
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.signal_tick.is_some() && matches!(self.exit_code, Some(0)) && self.drained_within_grace && matches!(self.previous_clean_shutdown, Some(true))
	}

	/// Serialize this verdict as a pretty JSON artifact.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// One phase of one card, as it was entered: a line of the run's
/// phase log. Recorded through the harness in phase order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRecord {
	/// Slug of the card this phase belongs to.
	pub card: String,
	/// The phase entered.
	pub phase: SoakPhase,
	/// Tick at which the phase was entered.
	pub at_tick: u64,
}

/// One card's recovery, as observed.
///
/// Carries its window, when recovery was observed (or never),
/// whether the driver's maintenance ran during the window, and
/// whether recovery landed inside it. Recorded through the harness
/// in card order. Carries no free text beyond the card's slug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRecord {
	/// Slug of the card that recovered (or did not).
	pub card: String,
	/// The card's recovery window.
	pub window: RecoveryWindow,
	/// Tick at which recovery was observed, or `None` when it never
	/// was inside the observation the caller supplied.
	pub recovered_at_tick: Option<u64>,
	/// The maintenance the card's recovery called for.
	pub maintenance: MaintenanceKind,
	/// Whether that maintenance ran during the recovery phase.
	pub maintenance_ran: bool,
}

impl RecoveryRecord {
	/// Whether recovery landed inside the window — and, for a card
	/// that called for maintenance, only counts when the maintenance
	/// ran: a recovery in which the daemons never resumed is not
	/// the recovery the deck asked for.
	#[must_use]
	pub const fn recovered(&self) -> bool {
		self.window.recovered_within(self.recovered_at_tick) && (matches!(self.maintenance, MaintenanceKind::None) || self.maintenance_ran)
	}
}

/// What the maintenance driver asks the caller to do next.
///
/// The driver performs none of these itself: the caller runs the
/// traffic, applies the failure, runs the maintenance and supplies
/// the observations, the same seam the oracle judges across.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckAction {
	/// Run steady traffic for this card's baseline.
	RunBaseline {
		/// Index of the current card.
		card: usize,
	},
	/// Apply this card's failure at its injection tick.
	Inject {
		/// Index of the current card.
		card: usize,
		/// The failure to apply.
		kind: FailureKind,
	},
	/// Run this maintenance during the recovery window.
	RunMaintenance {
		/// Index of the current card.
		card: usize,
		/// The maintenance to run.
		maintenance: MaintenanceKind,
	},
	/// Observe recovery inside this window.
	AwaitRecovery {
		/// Index of the current card.
		card: usize,
		/// The window recovery must land in.
		window: RecoveryWindow,
	},
	/// Judge this card: its recovery record, and for the SIGTERM
	/// card its verdict.
	Verify {
		/// Index of the current card.
		card: usize,
	},
	/// Every card is complete; the deck is done.
	Complete,
}

/// The maintenance driver: drives one [`FailureDeck`] card by card
/// through its phases, collecting the caller's observations into
/// phase records, recovery records and the SIGTERM verdict.
///
/// Pure and deterministic: the same deck and the same observations
/// produce the same records on any machine, so a deck run can be
/// replayed in a test with no server at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceDriver {
	deck: FailureDeck,
	card_index: usize,
	phase: SoakPhase,
	maintenance_ran: bool,
	recovery_observed: bool,
	recovered_at_tick: Option<u64>,
	sigterm: Option<SigtermVerdict>,
	phases: Vec<PhaseRecord>,
	recoveries: Vec<RecoveryRecord>,
}

impl MaintenanceDriver {
	/// Drive `deck` from its first card's baseline.
	#[must_use]
	pub const fn new(deck: FailureDeck) -> Self {
		Self { deck, card_index: 0, phase: SoakPhase::Baseline, maintenance_ran: false, recovery_observed: false, recovered_at_tick: None, sigterm: None, phases: Vec::new(), recoveries: Vec::new() }
	}

	/// The deck being driven.
	#[must_use]
	pub const fn deck(&self) -> &FailureDeck {
		&self.deck
	}

	/// Index of the current card (the deck's length once complete).
	#[must_use]
	pub const fn card_index(&self) -> usize {
		self.card_index
	}

	/// Phase of the current card.
	#[must_use]
	pub const fn phase(&self) -> SoakPhase {
		self.phase
	}

	/// The current card, or `None` once the deck is complete.
	#[must_use]
	pub fn current_card(&self) -> Option<&FailureCard> {
		self.deck.card(self.card_index)
	}

	/// Whether every card is complete.
	#[must_use]
	pub const fn is_complete(&self) -> bool {
		self.card_index >= self.deck.len()
	}

	/// The action the caller performs next, from the driver's state.
	#[must_use]
	pub fn action(&self) -> DeckAction {
		let Some(card) = self.current_card() else {
			return DeckAction::Complete;
		};
		match self.phase {
			SoakPhase::Baseline => DeckAction::RunBaseline { card: self.card_index },
			SoakPhase::Injection => DeckAction::Inject { card: self.card_index, kind: card.kind },
			SoakPhase::Recovery => {
				if card.maintenance != MaintenanceKind::None && !self.maintenance_ran {
					DeckAction::RunMaintenance { card: self.card_index, maintenance: card.maintenance }
				} else {
					DeckAction::AwaitRecovery { card: self.card_index, window: card.recovery_window() }
				}
			}
			SoakPhase::Verification => DeckAction::Verify { card: self.card_index },
			SoakPhase::Complete => DeckAction::Complete,
		}
	}

	/// Phase records so far, in the order phases were entered.
	#[must_use]
	pub fn phases(&self) -> &[PhaseRecord] {
		&self.phases
	}

	/// Recovery records so far, in card order.
	#[must_use]
	pub fn recoveries(&self) -> &[RecoveryRecord] {
		&self.recoveries
	}

	/// The SIGTERM verdict, once the SIGTERM card has been judged.
	#[must_use]
	pub const fn sigterm_verdict(&self) -> Option<&SigtermVerdict> {
		self.sigterm.as_ref()
	}

	/// Whether every completed card recovered inside its window.
	/// Vacuously true before any card completes — a caller
	/// publishing a deck run checks [`Self::is_complete`] beside
	/// this, as with the oracle's vacuous pass.
	#[must_use]
	pub fn all_recovered(&self) -> bool {
		self.recoveries.iter().all(RecoveryRecord::recovered)
	}

	/// Enter the current phase at `at_tick`: records the phase and
	/// advances the driver to the phase's successor state. The
	/// transitions the phases allow:
	///
	/// * `Baseline → Injection` — unconditional;
	/// * `Injection → Recovery` — unconditional (the caller applied
	///   the failure the [`DeckAction::Inject`] action named);
	/// * `Recovery → Verification` — only once recovery has been
	///   observed via [`Self::observe_recovery`] (observed inside
	///   the window or not — a missed window is a result, recorded,
	///   not a stuck driver) and the card's maintenance has run;
	/// * `Verification → Complete` — only once a SIGTERM card's
	///   verdict has been judged via [`Self::observe_sigterm`];
	///   completing the card records its recovery and advances to
	///   the next card's baseline.
	///
	/// # Errors
	///
	/// Returns an error when the deck is already complete, or when
	/// a gated transition's observation is still missing.
	pub fn advance(&mut self, at_tick: u64) -> anyhow::Result<()> {
		let card = self.current_card().cloned().ok_or_else(|| anyhow::anyhow!("the failure deck is already complete"))?;
		match self.phase {
			SoakPhase::Recovery => {
				anyhow::ensure!(self.recovery_observed, "recovery must be observed before the card can be verified");
				anyhow::ensure!(card.maintenance == MaintenanceKind::None || self.maintenance_ran, "maintenance {:?} must run during recovery before the card can be verified", card.maintenance);
			}
			SoakPhase::Verification => {
				if card.kind == FailureKind::Sigterm {
					anyhow::ensure!(self.sigterm.is_some(), "the SIGTERM verdict must be judged before the SIGTERM card can complete");
				}
			}
			SoakPhase::Complete => return Err(anyhow::anyhow!("the failure deck is already complete")),
			_ => {}
		}
		self.phases.push(PhaseRecord { card: card.id.clone(), phase: self.phase, at_tick });
		match self.phase.next() {
			Some(SoakPhase::Complete) | None => {
				self.phases.push(PhaseRecord { card: card.id.clone(), phase: SoakPhase::Complete, at_tick });
				self.recoveries.push(RecoveryRecord { card: card.id.clone(), window: card.recovery_window(), recovered_at_tick: self.recovered_at_tick, maintenance: card.maintenance, maintenance_ran: self.maintenance_ran });
				self.card_index += 1;
				self.phase = SoakPhase::Baseline;
				self.maintenance_ran = false;
				self.recovered_at_tick = None;
				self.recovery_observed = false;
			}
			Some(next) => {
				self.phase = next;
			}
		}
		Ok(())
	}

	/// Record that the current card's maintenance ran (its
	/// [`DeckAction::RunMaintenance`] action was performed). Only
	/// meaningful in the recovery phase.
	///
	/// # Errors
	///
	/// Returns an error when the driver is not in a recovery phase,
	/// or when the current card calls for no maintenance.
	pub fn run_maintenance(&mut self) -> anyhow::Result<MaintenanceKind> {
		anyhow::ensure!(self.phase == SoakPhase::Recovery, "maintenance runs during recovery, not {:?}", self.phase);
		let card = self.current_card().ok_or_else(|| anyhow::anyhow!("the failure deck is already complete"))?;
		anyhow::ensure!(card.maintenance != MaintenanceKind::None, "card '{}' calls for no maintenance", card.id);
		let maintenance = card.maintenance;
		self.maintenance_ran = true;
		Ok(maintenance)
	}

	/// Observe the current card's recovery: `recovered_at_tick` is
	/// the tick at which the caller observed the server recovered,
	/// or `None` when the window closed with no recovery observed.
	/// Only in the recovery phase, and only once per card.
	///
	/// # Errors
	///
	/// Returns an error when the driver is not in a recovery phase
	/// or recovery was already observed for this card.
	pub fn observe_recovery(&mut self, recovered_at_tick: Option<u64>) -> anyhow::Result<()> {
		anyhow::ensure!(self.phase == SoakPhase::Recovery, "recovery is observed during the recovery phase, not {:?}", self.phase);
		anyhow::ensure!(!self.recovery_observed, "recovery for this card has already been observed");
		self.recovery_observed = true;
		self.recovered_at_tick = recovered_at_tick;
		Ok(())
	}

	/// Judge the SIGTERM card's verdict from the caller's
	/// observation. Only in the SIGTERM card's verification phase,
	/// and only once for the deck.
	///
	/// # Errors
	///
	/// Returns an error when the current card is not a SIGTERM card
	/// in its verification phase, or a verdict was already judged.
	pub fn observe_sigterm(&mut self, observation: SigtermObservation) -> anyhow::Result<SigtermVerdict> {
		anyhow::ensure!(self.phase == SoakPhase::Verification, "the SIGTERM verdict is judged during verification, not {:?}", self.phase);
		let card = self.current_card().ok_or_else(|| anyhow::anyhow!("the failure deck is already complete"))?;
		anyhow::ensure!(card.kind == FailureKind::Sigterm, "card '{}' is not the SIGTERM card", card.id);
		anyhow::ensure!(self.sigterm.is_none(), "the SIGTERM verdict has already been judged");
		let verdict = SigtermVerdict::judge(observation);
		self.sigterm = Some(verdict);
		Ok(verdict)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn card(id: &str, kind: FailureKind, inject_tick: u64, recovery_ticks: u64, maintenance: MaintenanceKind) -> FailureCard {
		FailureCard { id: id.to_string(), kind, inject_tick, recovery_ticks, maintenance }
	}

	fn deck() -> FailureDeck {
		FailureDeck::new(vec![card("contention", FailureKind::MaintenanceContention, 10, 5, MaintenanceKind::Reconcile), card("restart", FailureKind::ServerRestart, 30, 5, MaintenanceKind::None), card("sigterm", FailureKind::Sigterm, 50, 5, MaintenanceKind::Backup)]).expect("valid deck")
	}

	fn graceful() -> SigtermObservation {
		SigtermObservation { signal_tick: Some(50), exit_code: Some(0), drained_within_grace: true, previous_clean_shutdown: Some(true) }
	}

	/// Drive one non-SIGTERM card to completion, recovering at
	/// `recovered_at`.
	fn finish_card(driver: &mut MaintenanceDriver, recovered_at: Option<u64>) {
		driver.advance(0).expect("baseline -> injection");
		driver.advance(0).expect("injection -> recovery");
		if driver.current_card().expect("card").maintenance != MaintenanceKind::None {
			driver.run_maintenance().expect("maintenance during recovery");
		}
		driver.observe_recovery(recovered_at).expect("observe recovery");
		driver.advance(0).expect("recovery -> verification");
		driver.advance(0).expect("verification -> complete");
	}

	#[test]
	fn the_standard_deck_is_valid_and_ends_with_sigterm() {
		let deck = FailureDeck::standard();
		assert_eq!(deck.len(), 3);
		assert_eq!(deck.card(2).expect("third card").kind, FailureKind::Sigterm, "SIGTERM ends the deck because it ends the server");
		assert_eq!(FailureDeck::new(deck.cards.clone()).expect("the standard deck passes validation"), deck);
		let json = deck.to_json().expect("serialize deck");
		assert_eq!(serde_json::from_str::<FailureDeck>(&json).expect("deserialize deck"), deck);
	}

	#[test]
	fn deck_validation_rejects_every_broken_rule() {
		assert!(FailureDeck::new(vec![]).is_err(), "an empty deck is not a deck");
		assert!(FailureDeck::new(vec![card("Bad Id", FailureKind::ServerRestart, 0, 5, MaintenanceKind::None)]).is_err(), "card ids are slugs");
		assert!(FailureDeck::new(vec![card("-lead", FailureKind::ServerRestart, 0, 5, MaintenanceKind::None)]).is_err(), "a slug does not start with '-'");
		assert!(FailureDeck::new(vec![card("a", FailureKind::ServerRestart, 0, 0, MaintenanceKind::None)]).is_err(), "a recovery window must be non-empty");
		assert!(FailureDeck::new(vec![card("a", FailureKind::ServerRestart, 0, 5, MaintenanceKind::None), card("a", FailureKind::ServerRestart, 10, 5, MaintenanceKind::None)]).is_err(), "card ids are unique");
		assert!(FailureDeck::new(vec![card("a", FailureKind::ServerRestart, 10, 5, MaintenanceKind::None), card("b", FailureKind::ServerRestart, 5, 5, MaintenanceKind::None)]).is_err(), "cards are in injection order");
		assert!(FailureDeck::new(vec![card("a", FailureKind::Sigterm, 0, 5, MaintenanceKind::None), card("b", FailureKind::ServerRestart, 10, 5, MaintenanceKind::None)]).is_err(), "SIGTERM must be last");
		assert!(FailureDeck::new(vec![card("a", FailureKind::ServerRestart, 0, 5, MaintenanceKind::None), card("b", FailureKind::Sigterm, 10, 5, MaintenanceKind::None)]).is_ok(), "one SIGTERM card, last, is the valid shape");
	}

	#[test]
	fn phases_form_one_ordered_chain() {
		let mut phase = SoakPhase::Baseline;
		let mut walked = vec![phase];
		while let Some(next) = phase.next() {
			phase = next;
			walked.push(phase);
		}
		assert_eq!(walked, SoakPhase::all(), "the chain walks every phase exactly once, in order");
		assert_eq!(SoakPhase::Complete.next(), None);
	}

	#[test]
	fn a_recovery_window_is_exact_at_its_edges() {
		let window = RecoveryWindow { start_tick: 10, tick_count: 5 };
		assert_eq!(window.end_tick(), 15);
		assert!(window.contains(10) && window.contains(14));
		assert!(!window.contains(9) && !window.contains(15), "the end tick is already outside");
		assert!(window.recovered_within(Some(14)));
		assert!(!window.recovered_within(Some(15)), "recovery at the end tick is too late, exactly");
		assert!(!window.recovered_within(None), "no recovery observed is no recovery");
	}

	#[test]
	fn the_sigterm_verdict_is_the_exact_conjunction() {
		assert!(SigtermVerdict::judge(graceful()).passed(), "sent, exit 0, drained in grace, clean shutdown: graceful");
		let mut obs = graceful();
		obs.signal_tick = None;
		assert!(!SigtermVerdict::judge(obs).passed(), "a signal never sent cannot pass vacuously");
		let mut obs = graceful();
		obs.exit_code = Some(70);
		assert!(!SigtermVerdict::judge(obs).passed(), "a non-zero exit is not graceful");
		let mut obs = graceful();
		obs.exit_code = None;
		assert!(!SigtermVerdict::judge(obs).passed(), "a process that never exited is not graceful");
		let mut obs = graceful();
		obs.drained_within_grace = false;
		assert!(!SigtermVerdict::judge(obs).passed(), "draining past the grace is not graceful");
		let mut obs = graceful();
		obs.previous_clean_shutdown = Some(false);
		assert!(!SigtermVerdict::judge(obs).passed(), "an unclean previous shutdown is not graceful");
		let mut obs = graceful();
		obs.previous_clean_shutdown = None;
		assert!(!SigtermVerdict::judge(obs).passed(), "an unobserved shutdown state is not graceful");
		let verdict = SigtermVerdict::judge(graceful());
		let json = verdict.to_json().expect("serialize verdict");
		assert_eq!(serde_json::from_str::<SigtermVerdict>(&json).expect("deserialize verdict"), verdict);
	}

	#[test]
	fn the_driver_walks_the_deck_in_phase_order() {
		let mut driver = MaintenanceDriver::new(deck());
		assert_eq!(driver.action(), DeckAction::RunBaseline { card: 0 });
		driver.advance(10).expect("baseline");
		assert_eq!(driver.action(), DeckAction::Inject { card: 0, kind: FailureKind::MaintenanceContention });
		driver.advance(10).expect("injection");
		assert_eq!(driver.action(), DeckAction::RunMaintenance { card: 0, maintenance: MaintenanceKind::Reconcile }, "the driver runs the card's maintenance first in recovery");
		assert!(driver.advance(11).is_err(), "recovery cannot end before recovery is observed");
		driver.run_maintenance().expect("run maintenance");
		assert_eq!(driver.action(), DeckAction::AwaitRecovery { card: 0, window: RecoveryWindow { start_tick: 10, tick_count: 5 } });
		driver.observe_recovery(Some(12)).expect("observe recovery");
		driver.advance(12).expect("recovery");
		assert_eq!(driver.action(), DeckAction::Verify { card: 0 });
		driver.advance(12).expect("verification");
		assert_eq!(driver.card_index(), 1);
		assert_eq!(driver.phase(), SoakPhase::Baseline, "the next card starts at its baseline");
		assert!(driver.all_recovered(), "recovery at tick 12 inside 10..15, with maintenance run");
		assert_eq!(driver.phases().len(), 5, "one card logs all five phases");
		assert_eq!(driver.phases().iter().map(|record| record.phase).collect::<Vec<_>>(), SoakPhase::all());
		assert_eq!(driver.recoveries().len(), 1);
		assert!(driver.recoveries()[0].recovered());
	}

	#[test]
	fn a_missed_window_and_skipped_maintenance_are_recorded_not_hidden() {
		let mut driver = MaintenanceDriver::new(deck());
		// Recovery observed after the window closed.
		driver.advance(10).expect("baseline");
		driver.advance(10).expect("injection");
		driver.run_maintenance().expect("maintenance");
		driver.observe_recovery(Some(99)).expect("late recovery is still an observation");
		driver.advance(99).expect("recovery");
		driver.advance(99).expect("verification");
		assert!(!driver.all_recovered(), "a recovery outside the window did not recover");
		assert!(!driver.recoveries()[0].recovered());

		// A card whose maintenance never ran cannot leave recovery.
		let mut driver = MaintenanceDriver::new(deck());
		driver.advance(10).expect("baseline");
		driver.advance(10).expect("injection");
		driver.observe_recovery(Some(12)).expect("observe recovery");
		assert!(driver.advance(12).is_err(), "the card's maintenance must run before verification");
	}

	#[test]
	fn the_driver_judges_sigterm_only_on_the_sigterm_card() {
		let mut driver = MaintenanceDriver::new(deck());
		finish_card(&mut driver, Some(12));
		finish_card(&mut driver, Some(32));
		assert_eq!(driver.current_card().expect("sigterm card").kind, FailureKind::Sigterm);
		driver.advance(50).expect("baseline");
		driver.advance(50).expect("injection");
		driver.run_maintenance().expect("backup during recovery");
		driver.observe_recovery(Some(52)).expect("observe recovery");
		driver.advance(52).expect("recovery");
		assert!(driver.advance(52).is_err(), "the SIGTERM card cannot complete before its verdict");
		let verdict = driver.observe_sigterm(graceful()).expect("judge the verdict");
		assert!(verdict.passed());
		assert_eq!(driver.sigterm_verdict(), Some(&verdict));
		driver.advance(52).expect("verification completes the deck");
		assert!(driver.is_complete());
		assert_eq!(driver.action(), DeckAction::Complete);
		assert!(driver.advance(52).is_err(), "a complete deck advances no further");
		assert_eq!(driver.recoveries().len(), 3, "every card recorded its recovery");
		assert!(driver.all_recovered());
		assert_eq!(driver.phases().len(), 15, "three cards, five phases each");
	}

	#[test]
	fn driver_observations_are_phase_gated() {
		let mut driver = MaintenanceDriver::new(deck());
		assert!(driver.run_maintenance().is_err(), "no maintenance in the baseline phase");
		assert!(driver.observe_recovery(Some(1)).is_err(), "no recovery observation in the baseline phase");
		assert!(driver.observe_sigterm(graceful()).is_err(), "no SIGTERM verdict in the baseline phase");
		driver.advance(10).expect("baseline");
		driver.advance(10).expect("injection");
		assert!(driver.observe_sigterm(graceful()).is_err(), "no SIGTERM verdict during recovery");
	}
}
