//! The crash matrix (D-S21, SOA-9 `crash_kill9`): the subprocess SIGKILL soak
//! as one judged matrix, not three one-off bench workloads.
//!
//! The failure tests in [`crate::failure`] already kill a real child process
//! with `SIGKILL` mid-ingest and mid-compaction and flip a byte of a closed
//! segment. What they do not give is a soak verdict: one run, every failure,
//! every seed, judged as a whole. That is what this module adds. A matrix run
//! is [`Failure::ALL`] crossed with the caller's seeds — each cell is one
//! [`run_failure`] call, so the kill points, the
//! corrupted byte and the reopen gate are the failure tests' own, and a cell
//! passes exactly when its reopened store holds every acknowledged row exactly
//! (and quarantines, never serves, a damaged segment). The matrix passes only
//! when every cell does; a single failing cell fails the run, because a crash
//! matrix with a hole in it is not evidence.
//!
//! The matrix deliberately does not add a new kill mechanism, a new store
//! shape, or a power-loss claim: a kill soak cannot see a missing fsync, so
//! the privileged power-loss validation stays the owner task the 1.0 plan
//! names (decision 20), and the fault-injection crash matrix in
//! `weftdb`'s `swap_tests` stays the in-process matrix this one complements.
//! `scripts/crash-matrix.sh` runs this mode plus those tests and stages the
//! report through EVI-4's artifact pipeline; `.github/workflows/crash-matrix.yml`
//! runs the script on a schedule and on crash-path pull requests.

use crate::{
	failure::{run_failure, Failure, FailureParams}, ingest::IngestProfile, schema::BenchResult
};

/// One judged cell of the matrix: one failure at one seed.
#[derive(Debug)]
pub struct MatrixCell {
	/// The failure this cell injected.
	pub failure: Failure,
	/// The seed this cell's kill points and corrupted byte derive from.
	pub seed: u64,
	/// The failure test's result for this cell.
	pub result: BenchResult,
}

impl MatrixCell {
	/// A cell passes exactly when its failure test's gate passed.
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.result.correctness.passed()
	}
}

/// Run a specific set of failures at every seed, in failure-major order (all seeds
/// of one failure before the next).
///
/// Unlike [`run_matrix`] (which always runs [`Failure::ALL`]), this variant accepts
/// an explicit list of failures so callers can exclude SIGKILL variants on platforms
/// where the upstream engine refuses to reopen after a mid-checkpoint SIGKILL
/// (see <https://github.com/tursodatabase/turso/issues/7480>).
///
/// # Errors
///
/// An empty seed list, or any failure test that cannot start.
pub async fn run_matrix_cells(profile: &IngestProfile, timestamps: &[i64], values: &[bigdecimal::BigDecimal], value_scale: u8, params: &FailureParams, seeds: &[u64], failures: &[Failure]) -> anyhow::Result<Vec<MatrixCell>> {
	anyhow::ensure!(!seeds.is_empty(), "a crash matrix needs at least one seed");
	let mut cells = Vec::with_capacity(failures.len() * seeds.len());
	for &failure in failures {
		for &seed in seeds {
			let cell_params = FailureParams { seed, ..params.clone() };
			let result = run_failure(failure, profile, timestamps, values, value_scale, &cell_params).await.map_err(|e| anyhow::anyhow!("crash-matrix cell {} seed {seed} failed to run: {e:#}", failure.as_str()))?;
			cells.push(MatrixCell { failure, seed, result });
		}
	}
	Ok(cells)
}

/// Run the matrix: every failure in [`Failure::ALL`], at every seed, in
/// failure-major order (all seeds of one failure before the next, so the
/// per-cell lines group by failure).
///
/// This is the convenience wrapper over [`run_matrix_cells`] that preserves
/// existing callers — it always passes [`Failure::ALL`].
///
/// # Errors
///
/// An empty seed list (a matrix with no column is vacuous, and a vacuous
/// pass is the one verdict a crash matrix must never give), or any failure
/// test that fails to run — a store that misbehaves is not an error, it is
/// a failing cell, and surfaces through the cell's gate, not here.
pub async fn run_matrix(profile: &IngestProfile, timestamps: &[i64], values: &[bigdecimal::BigDecimal], value_scale: u8, params: &FailureParams, seeds: &[u64]) -> anyhow::Result<Vec<MatrixCell>> {
	run_matrix_cells(profile, timestamps, values, value_scale, params, seeds, &Failure::ALL).await
}

/// The matrix passes only when every cell does.
#[must_use]
pub fn all_passed(cells: &[MatrixCell]) -> bool {
	!cells.is_empty() && cells.iter().all(MatrixCell::passed)
}
