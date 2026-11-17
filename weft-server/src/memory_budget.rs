//! The container memory budget (ROB-14, cut 1): read the cgroup limit, hold it in server state, and account with an RAII reservation guard.
//!
//! ## Read order (cut 1, not yet enforced)
//!
//! 1. `WEFT_MEMORY_BUDGET_BYTES` — the operator's **lower** target — combined
//!    with the cgroup value as `min(override, cgroup)`. Unset, `0`, `max` or
//!    whitespace-only means no budget (`limit: None`).
//! 2. cgroup v2: `<base>/memory.max` (and `current_usage_at` reads
//!    `<base>/memory.current` for observability). Falling back to:
//! 3. cgroup v1: `<base>/memory/memory.limit_in_bytes` (and
//!    `<base>/memory/memory.usage_in_bytes` for observability).
//!
//! A platform that hides the cgroup limit (absent or unreadable files) disables
//! the budget — never denies everything.
//!
//! ## Why `weft-server`, not `weftdb`
//!
//! The cgroup limit is a server deployment concern. A published library must not
//! read `/sys/fs/cgroup`, so the reader lives behind a plain file read here. `libc`
//! stays unused in this cut.

use std::sync::{
	atomic::{AtomicU64, Ordering}, Arc
};

/// An in-memory memory budget that accounts bytes with an [`Arc<AtomicU64>`]
/// counter and an RAII reservation guard.
///
/// `limit: None` means unbounded — every reservation succeeds.
#[derive(Clone)]
pub struct MemoryBudget {
	/// The upper byte bound, or `None` when no budget is in effect.
	pub limit: Option<u64>,
	/// Bytes currently reserved.
	reserved: Arc<AtomicU64>,
}

/// The RAII guard a successful [`MemoryBudget::try_reserve`] returns.
///
/// On [`Drop`] it releases its byte count from the budget's counter, so a panic
/// (or an early return) cannot leak the reservation.
#[derive(Debug)]
pub struct Reservation {
	/// How many bytes were reserved.
	pub bytes: u64,
	/// The shared counter to release from.
	reserved: Arc<AtomicU64>,
}

impl Reservation {
	/// The byte count this reservation holds.
	#[must_use]
	pub const fn bytes(&self) -> u64 {
		self.bytes
	}
}

impl Drop for Reservation {
	fn drop(&mut self) {
		self.reserved.fetch_sub(self.bytes, Ordering::Relaxed);
	}
}

/// The error returned when a reservation would exceed the budget.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ReserveError {
	/// The budget is exceeded: `reserved + wanted > limit`.
	OverBudget {
		/// The budget's limit.
		limit: u64,
		/// Bytes already reserved (at decision time).
		reserved: u64,
		/// The bytes the caller wanted.
		wanted: u64,
	},
}

impl std::default::Default for MemoryBudget {
	fn default() -> Self {
		Self::new(None)
	}
}

impl MemoryBudget {
	/// Build a new memory budget.
	///
	/// `limit` is `None` (unbounded) or the byte ceiling.
	#[must_use]
	pub fn new(limit: Option<u64>) -> Self {
		Self { limit, reserved: Arc::new(AtomicU64::new(0)) }
	}

	/// The budget's limit, or `None` when unbounded.
	#[must_use]
	pub const fn limit(&self) -> Option<u64> {
		self.limit
	}

	/// Bytes currently reserved (for diagnostic / metrics use).
	#[must_use]
	pub fn reserved(&self) -> u64 {
		self.reserved.load(Ordering::Relaxed)
	}

	/// Try to reserve `wanted` bytes against this budget.
	///
	/// Returns [`Ok`] with a [`Reservation`] when the budget has room (or is
	/// unbounded), or [`Err`] with [`ReserveError::OverBudget`] when the
	/// reservation would exceed the limit.
	///
	/// The reservation is **always** added to the counter before returning —
	/// the caller holds the bytes from the moment this returns `Ok`.
	///
	/// # Errors
	///
	/// [`ReserveError::OverBudget`] when the budget is bounded and this
	/// reservation would push the counter past the limit. The counter is rolled
	/// back before the error is returned, so a refused reservation holds nothing.
	pub fn try_reserve(&self, wanted: u64) -> Result<Reservation, ReserveError> {
		self.limit.map_or_else(
			|| {
				// Unbounded: reserve succeeds but we still account for it.
				self.reserved.fetch_add(wanted, Ordering::Relaxed);
				Ok(Reservation { bytes: wanted, reserved: self.reserved.clone() })
			},
			|limit| {
				// The decision is based on the value *after* adding wanted, so
				// concurrent reservations cannot overshoot.
				let reserved = self.reserved.fetch_add(wanted, Ordering::Relaxed);
				if reserved + wanted <= limit {
					Ok(Reservation { bytes: wanted, reserved: self.reserved.clone() })
				} else {
					// Overshoot: roll back by subtracting what we just added.
					self.reserved.fetch_sub(wanted, Ordering::Relaxed);
					Err(ReserveError::OverBudget { limit, reserved, wanted })
				}
			},
		)
	}
}

// ---------------------------------------------------------------------------
// Cgroup limit reading
// ---------------------------------------------------------------------------

/// Read the cgroup memory limit from the environment, combined with the
/// operator's override `op`.
///
/// Rule: `min(op, cgroup)` when both are set; either alone is used as-is;
/// `None` is unbounded.
#[must_use]
fn combine(op: Option<u64>, cgroup: Option<u64>) -> Option<u64> {
	match (op, cgroup) {
		(Some(o), Some(c)) => Some(o.min(c)),
		(Some(o), None) => Some(o),
		(None, Some(c)) => Some(c),
		(None, None) => None,
	}
}

/// Parse a string value from the operator's `WEFT_MEMORY_BUDGET_BYTES`.
///
/// `None`/`""`/`"max"`/`"0"`/whitespace-garbage means `None` (no budget).
/// A positive integer means `Some(n)`.
#[must_use]
pub fn parse_override(value: Option<&str>) -> Option<u64> {
	let trimmed = value.and_then(|s| {
		let t = s.trim();
		if t.is_empty() {
			None
		} else {
			Some(t)
		}
	});
	match trimmed {
		None => None,
		Some(s) if s == "max" || s == "0" => None,
		Some(s) => {
			let n = s.parse::<u64>().ok();
			// A negative or unparseable value means no budget.
			if n == Some(0) {
				None
			} else {
				n
			}
		}
	}
}

/// Parse a string from a cgroup limit file.
///
/// `"max"` or an unparseable/unreadable line means `None`.
#[must_use]
fn cgroup_limit_from(value: Option<&str>) -> Option<u64> {
	value.and_then(|s| {
		let trimmed = s.trim();
		if trimmed.is_empty() || trimmed == "max" {
			None
		} else {
			trimmed.parse::<u64>().ok()
		}
	})
}

/// Read the cgroup memory limit at the filesystem `base`.
///
/// Tries cgroup v2 first (`<base>/memory.max`), then falls back to v1
/// (`<base>/memory/memory.limit_in_bytes`). When neither file exists or
/// both are unreadable, returns `None`.
///
/// This is deterministic and filesystem-only — never reads the real
/// `/sys/fs/cgroup`.
#[must_use]
pub fn read_cgroup_limit_at(base: &std::path::Path) -> Option<u64> {
	// v2: <base>/memory.max.
	let v2_path = base.join("memory.max");
	if v2_path.is_file() {
		// A file we cannot read, or one whose contents do not parse, does not end
		// the search. "No limit" and "limit unreadable" must not collapse into the
		// same answer: refusing the budget is the safe reading of an unreadable
		// file only in the sense that nothing is enforced, so keep looking.
		if let Ok(text) = std::fs::read_to_string(&v2_path) {
			if let Some(limit) = cgroup_limit_from(Some(text.trim())) {
				return Some(limit);
			}
		}
	}
	// v1 fallback: <base>/memory/memory.limit_in_bytes.
	let v1_path = base.join("memory").join("memory.limit_in_bytes");
	if v1_path.is_file() {
		if let Ok(text) = std::fs::read_to_string(&v1_path) {
			return cgroup_limit_from(Some(text.trim()));
		}
	}
	None
}

/// The cgroup's current memory usage at `base`, for observability only.
///
/// Tries cgroup v2 (`<base>/memory.current`), then v1
/// (`<base>/memory/memory.usage_in_bytes`). `None` when neither is readable:
/// usage is never part of an admission decision in cut 1, so a missing value
/// costs a diagnostic and nothing else.
#[must_use]
pub fn current_usage_at(base: &std::path::Path) -> Option<u64> {
	let v2_path = base.join("memory.current");
	if v2_path.is_file() {
		if let Ok(text) = std::fs::read_to_string(&v2_path) {
			if let Some(usage) = cgroup_limit_from(Some(text.trim())) {
				return Some(usage);
			}
		}
	}
	let v1_path = base.join("memory").join("memory.usage_in_bytes");
	if v1_path.is_file() {
		if let Ok(text) = std::fs::read_to_string(&v1_path) {
			return cgroup_limit_from(Some(text.trim()));
		}
	}
	None
}

/// Read the cgroup limit from the default path `/sys/fs/cgroup`.
///
/// Returns `None` when the filesystem path is absent or unreadable.
#[must_use]
pub fn read_cgroup_limit() -> Option<u64> {
	read_cgroup_limit_at(std::path::Path::new("/sys/fs/cgroup"))
}

/// Build a [`MemoryBudget`] from the operator's override and the cgroup limit.
///
/// 1. Parse `WEFT_MEMORY_BUDGET_BYTES` (the override).
/// 2. Read the cgroup limit from the default path.
/// 3. Combine: `min(override, cgroup)`.
///
/// When both are `None`, the budget is unbounded.
#[must_use]
pub fn budget_from_env() -> MemoryBudget {
	let op = parse_override(std::env::var("WEFT_MEMORY_BUDGET_BYTES").ok().as_deref());
	let cgroup = read_cgroup_limit();
	let limit = combine(op, cgroup);
	MemoryBudget::new(limit)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use std::path::Path;

	use tempfile::TempDir;

	use super::*;

	// -- parse_override --------------------------------------------------------

	#[test]
	fn parse_override_returns_none_for_empty_max_zero_or_garbage() {
		assert_eq!(parse_override(None), None);
		assert_eq!(parse_override(Some("")), None);
		assert_eq!(parse_override(Some("max")), None);
		assert_eq!(parse_override(Some("0")), None);
		assert_eq!(parse_override(Some("-1")), None);
		assert_eq!(parse_override(Some("abc")), None);
		assert_eq!(parse_override(Some("   ")), None);
		// Also 0 with whitespace.
		assert_eq!(parse_override(Some(" 0 ")), None);
	}

	#[test]
	fn parse_override_returns_some_for_a_positive_integer() {
		assert_eq!(parse_override(Some("1073741824")), Some(1_073_741_824));
	}

	#[test]
	fn parse_override_accepts_surrounding_whitespace() {
		assert_eq!(parse_override(Some("  1073741824  ")), Some(1_073_741_824));
	}

	// -- cgroup_limit_from -----------------------------------------------------

	#[test]
	fn cgroup_limit_from_returns_none_for_max_empty_or_garbage() {
		assert_eq!(cgroup_limit_from(None), None);
		assert_eq!(cgroup_limit_from(Some("")), None);
		assert_eq!(cgroup_limit_from(Some("max")), None);
		assert_eq!(cgroup_limit_from(Some("abc")), None);
	}

	#[test]
	fn cgroup_limit_from_returns_some_for_a_positive_integer() {
		assert_eq!(cgroup_limit_from(Some("1073741824")), Some(1_073_741_824));
	}

	// -- combine ---------------------------------------------------------------

	#[test]
	fn combine_uses_min_when_both_are_set() {
		assert_eq!(combine(Some(100), Some(200)), Some(100));
		assert_eq!(combine(Some(200), Some(100)), Some(100));
	}

	#[test]
	fn combine_uses_one_when_the_other_is_none() {
		assert_eq!(combine(Some(100), None), Some(100));
		assert_eq!(combine(None, Some(100)), Some(100));
		assert_eq!(combine(None, None), None);
	}

	// -- read_cgroup_limit_at --------------------------------------------------

	#[test]
	fn read_cgroup_limit_at_reads_v2_file() {
		let dir = TempDir::new().unwrap();
		let base = Path::new(dir.path());
		// Write v2 memory.max.
		std::fs::write(base.join("memory.max"), "4294967296");
		assert_eq!(read_cgroup_limit_at(base), Some(4_294_967_296));
	}

	#[test]
	fn read_cgroup_limit_at_falls_back_to_v1_when_v2_is_absent() {
		let dir = TempDir::new().unwrap();
		let base = Path::new(dir.path());
		// Write v1 memory.limit_in_bytes.
		std::fs::create_dir(base.join("memory"));
		std::fs::write(base.join("memory").join("memory.limit_in_bytes"), "8589934592");
		assert_eq!(read_cgroup_limit_at(base), Some(8_589_934_592));
	}

	#[test]
	fn read_cgroup_limit_at_returns_none_for_max_value() {
		let dir = TempDir::new().unwrap();
		let base = Path::new(dir.path());
		std::fs::write(base.join("memory.max"), "max\n");
		assert_eq!(read_cgroup_limit_at(base), None);
	}

	#[test]
	fn read_cgroup_limit_at_skips_a_v2_path_that_is_not_a_regular_file() {
		// A directory cannot hold a limit: `is_file` refuses it, and the v1
		// fallback still answers. A limit read from a non-file would be a bug
		// that only a container with a bind-mounted directory would show.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("memory")).expect("v1 dir");
		std::fs::create_dir(dir.path().join("memory.max")).expect("v2 non-file");
		std::fs::write(dir.path().join("memory").join("memory.limit_in_bytes"), "536870912\n").expect("v1 limit");
		assert_eq!(read_cgroup_limit_at(dir.path()), Some(536_870_912));
	}

	#[test]
	fn current_usage_at_reads_v2_then_v1() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("memory.current"), "1048576\n").expect("v2 usage");
		assert_eq!(current_usage_at(dir.path()), Some(1_048_576));
		let other = TempDir::new().unwrap();
		std::fs::create_dir_all(other.path().join("memory")).expect("v1 dir");
		std::fs::write(other.path().join("memory").join("memory.usage_in_bytes"), "2097152\n").expect("v1 usage");
		assert_eq!(current_usage_at(other.path()), Some(2_097_152));
		let empty = TempDir::new().unwrap();
		assert_eq!(current_usage_at(empty.path()), None);
	}

	#[test]
	fn read_cgroup_limit_at_returns_none_when_no_cgroup_file_exists() {
		let dir = TempDir::new().unwrap();
		let base = Path::new(dir.path());
		assert_eq!(read_cgroup_limit_at(base), None);
	}

	// -- MemoryBudget accounting -----------------------------------------------

	#[test]
	fn try_reserve_under_limit_succeeds_and_reserved_moves() {
		let budget = MemoryBudget::new(Some(100));
		assert_eq!(budget.reserved(), 0);
		let _ = budget.try_reserve(40).unwrap();
		assert_eq!(budget.reserved(), 40);
		let _ = budget.try_reserve(30).unwrap();
		assert_eq!(budget.reserved(), 70);
	}

	#[test]
	fn try_reserve_exactly_at_limit_succeeds() {
		let budget = MemoryBudget::new(Some(100));
		let _ = budget.try_reserve(100).unwrap();
		assert_eq!(budget.reserved(), 100);
	}

	#[test]
	fn try_reserve_one_byte_over_fails_with_over_budget_and_leaves_reserved_unchanged() {
		let budget = MemoryBudget::new(Some(100));
		let _ = budget.try_reserve(50).unwrap();
		assert_eq!(budget.reserved(), 50);
		let err = budget.try_reserve(51).expect_err("one byte over the limit");
		match err {
			ReserveError::OverBudget { limit, reserved, wanted } => {
				assert_eq!(limit, 100);
				assert_eq!(reserved, 50);
				assert_eq!(wanted, 51);
			}
		}
		// The failed reservation left the counter unchanged.
		assert_eq!(budget.reserved(), 50);
	}

	#[test]
	fn dropped_reservation_restores_the_counter() {
		let budget = MemoryBudget::new(Some(100));
		{
			let reservation = budget.try_reserve(60).unwrap();
			assert_eq!(budget.reserved(), 60);
			drop(reservation);
		}
		assert_eq!(budget.reserved(), 0);
	}

	#[test]
	fn unbounded_budget_never_fails() {
		let budget = MemoryBudget::new(None);
		let _ = budget.try_reserve(u64::MAX).unwrap();
		assert_eq!(budget.reserved(), u64::MAX);
	}

	#[test]
	fn two_concurrent_reservations_refuse_the_second_when_the_budget_is_exhausted() {
		let budget = MemoryBudget::new(Some(100));
		let _ = budget.try_reserve(60).unwrap();
		// The next reservation of 60 would push it to 120.
		let err = budget.try_reserve(60).expect_err("second reserve exceeds the budget");
		match err {
			ReserveError::OverBudget { .. } => { /* expected */ }
		}
		assert_eq!(budget.reserved(), 60);
	}
}
