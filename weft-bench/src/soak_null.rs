//! The null server for the soak harness (1.0 plan SOA-3, W2).
//!
//! SOA-5 will supply the soak ingest client that drives a live server; until
//! then — and whenever a run must exercise the harness without a server at
//! all — the scheduler needs a server-shaped object that accepts every step
//! and does no work. That is the null server: it lets the harness, the
//! scheduler, the interval logs and the self-monitoring all run end to end
//! (SOA-3's acceptance: "the harness runs without a live server") with no
//! process, port, or store behind it.
//!
//! The seam is the [`SoakServer`] trait, not the null server itself: SOA-5's
//! client implements this trait for a live server, and the scheduler only
//! ever sees the trait, so swapping the null server for a live one changes
//! no scheduler code. A server reports whether it is live
//! ([`SoakServer::is_live`]) so a run record can never mistake a null run
//! for a live-server run; the null server's answer is always `false`.

/// A server a soak step is submitted to.
///
/// The scheduler calls [`SoakServer::handle`] once per scheduled tick with
/// the tick index. The implementation performs the step's server work (or,
/// for the null server, none) and returns an error when the step failed —
/// the scheduler then aborts the harness, so a failed step can never read
/// as a completed tick.
pub trait SoakServer {
	/// Stable name of this server (`"null"` for [`NullServer`]).
	fn name(&self) -> &str;

	/// Whether this server is a live server. The null server answers
	/// `false`; a live-server implementation answers `true`.
	fn is_live(&self) -> bool {
		false
	}

	/// Perform the work for tick `tick`.
	///
	/// # Errors
	///
	/// Returns an error when the step failed.
	fn handle(&mut self, tick: u64) -> anyhow::Result<()>;
}

/// The null server: accepts every step, does no work, counts the steps it
/// accepted.
///
/// It holds no port, no process handle, and no store: there is nothing to
/// start, stop, or clean up, which is the point — a harness run against
/// the null server exercises everything except a live server.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullServer {
	/// Steps accepted so far.
	steps_accepted: u64,
}

impl NullServer {
	/// A null server that has accepted no steps yet.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Steps accepted so far.
	#[must_use]
	pub const fn steps_accepted(&self) -> u64 {
		self.steps_accepted
	}
}

impl SoakServer for NullServer {
	fn name(&self) -> &str {
		"null"
	}

	fn is_live(&self) -> bool {
		false
	}

	fn handle(&mut self, _tick: u64) -> anyhow::Result<()> {
		self.steps_accepted = self.steps_accepted.saturating_add(1);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn null_server_accepts_every_step_and_counts_them() {
		let mut server = NullServer::new();
		assert_eq!(server.name(), "null");
		assert!(!server.is_live(), "a null run must never read as a live-server run");
		assert_eq!(server.steps_accepted(), 0);
		for tick in 0..5 {
			server.handle(tick).expect("null server never fails a step");
		}
		assert_eq!(server.steps_accepted(), 5);
	}
}
