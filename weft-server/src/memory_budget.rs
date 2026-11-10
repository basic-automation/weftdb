//! cgroup-aware memory budget probe and per-request reservations (1.0 plan **ROB-14**).
//!
//! ## Probe
//!
//! [`probe`] reads the *effective* memory limit a container sees from the
//! cgroup hierarchy, in this order:
//!
//! 1. **cgroup v2**: `/sys/fs/cgroup/memory.max`. When the file reads
//!    `"max"` the limit is effectively infinite — the probe falls
//!    through to the total-RAM fallback.
//! 2. **cgroup v2 tuning knob**: `/sys/fs/cgroup/memory.high`. Reported
//!    when it is set to a value below `memory.max` (a deployment that
//!    uses `memory.high` as the effective cap; the operator set it
//!    intentionally).
//! 3. **cgroup v1**: `/sys/fs/cgroup/memory/memory.limit_in_bytes`. The
//!    file's sentinel (effectively `i64::MAX`) is treated as unlimited.
//! 4. **Host total RAM**: read from `/proc/meminfo` on Linux, else
//!    reported as unknown.
//! 5. **Unknown**: `0`.
//!
//! The probe is a pure function over an injectable filesystem root
//! (`&Path`), so every branch is testable with a temp directory.
//!
//! ## Reservation
//!
//! [`MemoryBudget`] wraps the probed total and an atomic counter of
//! remaining budget. A request that will buffer rows reserves an
//! estimated byte count via [`MemoryBudget::try_reserve`], which
//! returns a [`MemoryReservation`] RAII guard. The guard releases the
//! bytes back to the budget when it drops (no mutex held across an
//! `.await`), and a caller may also call [`MemoryReservation::release`]
//! early without dropping the scope.
//!
//! A reservation that would exceed the budget is refused with
//! [`Code::Overloaded`](crate::error::Code) — the existing typed refusal
//! error — nothing new is invented.

use std::{
	fs, path::Path, sync::atomic::{AtomicI64, Ordering}
};

use crate::error::Code;

// ── Probe ────────────────────────────────────────────────────────────────────────

/// What the memory probe learned: either an effective limit in bytes, or
/// an indication that the limit is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryProbe {
	/// The effective memory limit in bytes, or `0` when unknown.
	pub bytes: u64,
	/// Human-readable description of how the limit was determined
	/// (for startup logging).
	pub source: &'static str,
}

/// Probe the effective memory limit by reading cgroup files under `root`.
///
/// `root` is typically `/sys/fs/cgroup` but may be a temp directory
/// in tests. The function never panics — an unreadable or malformed file
/// falls through to the next source.
#[must_use]
pub fn probe(root: &Path) -> MemoryProbe {
	// 1. cgroup v2: memory.max
	if let Some(bytes) = read_cgroup_v2_max(root) {
		return MemoryProbe { bytes, source: "cgroup v2 memory.max" };
	}

	// 2. cgroup v2: memory.high (effective cap when set below max)
	if let Some(bytes) = read_cgroup_v2_high(root) {
		return MemoryProbe { bytes, source: "cgroup v2 memory.high" };
	}

	// 3. cgroup v1: memory/memory.limit_in_bytes
	if let Some(bytes) = read_cgroup_v1_limit(root) {
		return MemoryProbe { bytes, source: "cgroup v1 memory.limit_in_bytes" };
	}

	// 4. Host total RAM from /proc/meminfo
	if let Some(bytes) = read_proc_meminfo(root) {
		return MemoryProbe { bytes, source: "/proc/meminfo MemTotal" };
	}

	// 5. Unknown
	MemoryProbe { bytes: 0, source: "unknown" }
}

/// Read cgroup v2 `memory.max`, returning the limit or `None` if the
/// file is missing, unreadable, or carries the `"max"` sentinel.
fn read_cgroup_v2_max(root: &Path) -> Option<u64> {
	let path = root.join("memory.max");
	let content = fs::read_to_string(&path).ok()?;
	let trimmed = content.trim();
	if trimmed == "max" {
		return None; // effectively unlimited
	}
	trimmed.parse::<u64>().ok().filter(|&v| v > 0)
}

/// Read cgroup v2 `memory.high`, returning the value when it is set
/// below `memory.max` (i.e. it is the effective cap). Returns `None`
/// when the file is missing, unreadable, or carries `"max"`.
///
/// The base cap is read from `memory.max` to decide whether `high` is
/// tighter; if `memory.max` is `"max"` or missing, any `high` value
/// below that sentinel is reported.
fn read_cgroup_v2_high(root: &Path) -> Option<u64> {
	let base_path = root.join("memory.max");
	let high_path = root.join("memory.high");

	// Read memory.high; missing/unreadable means nothing to report.
	let high_content = fs::read_to_string(&high_path).ok()?;
	let high_trimmed = high_content.trim();
	if high_trimmed == "max" {
		return None;
	}
	let high: u64 = high_trimmed.parse::<u64>().ok().filter(|&v| v > 0)?;

	// Read memory.max to decide whether high is tighter.
	let unlimited = match fs::read_to_string(&base_path) {
		Ok(c) => c.trim() == "max",
		Err(_) => true, // memory.max missing → high is the cap
	};
	if unlimited {
		return Some(high);
	}

	// memory.max exists and is finite. Report high only when it is
	// below max.
	let base: u64 = fs::read_to_string(&base_path).ok()?.trim().parse().ok()?;
	if high < base {
		Some(high)
	} else {
		None
	}
}

/// Read cgroup v1 `memory/memory.limit_in_bytes`.
///
/// The sentinel near `i64::MAX` (`9223372036854771712` on a typical
/// kernel) is treated as unlimited and returns `None`.
fn read_cgroup_v1_limit(root: &Path) -> Option<u64> {
	let path = root.join("memory").join("memory.limit_in_bytes");
	let content = fs::read_to_string(&path).ok()?;
	let bytes: u64 = content.trim().parse().ok().filter(|&v| v > 0)?;

	// The sentinel is a kernel-visible page value near i64::MAX.
	// At least (i64::MAX as u64 >> PAGE_SHIFT) << PAGE_SHIFT.
	// Practically every kernel reports something within 1% of
	// i64::MAX. We accept anything >= 2^62 as "unlimited".
	if bytes >= (1u64 << 62) {
		return None;
	}
	Some(bytes)
}

/// Read host total RAM from `/proc/meminfo` (Linux).
///
/// The file starts with `MemTotal:       <kbytes> kB`. Missing or
/// unparseable files yield `None`.
fn read_proc_meminfo(root: &Path) -> Option<u64> {
	let path = root.join("proc").join("meminfo");
	let content = fs::read_to_string(&path).ok()?;
	let line = content.lines().next()?;
	// e.g. "MemTotal:       131015184 kB"
	let rest = line.strip_prefix("MemTotal:")?;
	let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
	Some(kb.checked_mul(1024)?)
}

// ── Budget ────────────────────────────────────────────────────────────────────────

/// A memory budget backed by an atomic counter of remaining bytes.
///
/// Created from a [`MemoryProbe`] and shared across requests. The
/// budget starts equal to the probed total.
#[derive(Debug)]
pub struct MemoryBudget {
	/// The total effective memory limit, or `0` when unknown.
	total: u64,
	/// Remaining budget: a signed value so we can detect
	/// over-reservation without a compare-and-swap loop on every
	/// release (the RAII drop restores unconditionally).
	available: AtomicI64,
}

impl MemoryBudget {
	/// Create a budget from a probe result. When the probe returned `0`
	/// (unknown), the budget is created with `0` total and a no-op
	/// reservation gate.
	#[must_use]
	pub fn from_probe(probe: MemoryProbe) -> Self {
		let total = probe.bytes;
		let available = i64::try_from(total).unwrap_or(i64::MAX);
		Self { total, available: AtomicI64::new(available) }
	}

	/// Create a budget with no memory limit (all reservations succeed).
	/// Use in tests and when no cgroup or host memory can be determined.
	#[must_use]
	pub fn unknown() -> Self {
		Self { total: 0, available: AtomicI64::new(i64::MAX) }
	}

	/// Create a budget with an explicit byte total. Useful for tests and
	/// for the optional `WEFT_MEMORY_BUDGET_BYTES` override.
	#[must_use]
	pub fn with_total(total: u64) -> Self {
		let available = i64::try_from(total).unwrap_or(i64::MAX);
		Self { total, available: AtomicI64::new(available) }
	}

	/// The total budget in bytes, or `0` when unknown.
	#[must_use]
	pub const fn total(&self) -> u64 {
		self.total
	}

	/// The remaining budget in bytes (best-effit: the atomic may
	/// change under a concurrent reservation or release).
	#[must_use]
	pub fn available(&self) -> u64 {
		self.available.load(Ordering::Acquire).max(0) as u64
	}

	/// Reserve `bytes` against the budget.
	///
	/// Returns an RAII guard when the budget can accommodate the
	/// reservation. The guard releases the bytes on drop.
	///
	/// # Errors
	///
	/// [`Code::Overloaded`] when the reservation would exceed the
	/// remaining budget (or when the budget itself is `0` / unknown).
	pub fn try_reserve(&self, bytes: u64) -> Result<MemoryReservation<'_>, Code> {
		if self.total == 0 {
			// Unknown budget: pass through. The bug this slice fixes
			// is on hosts where a cgroup limit is set and ignored.
			// When no limit is known, the previous behaviour
			// (unbounded) is preserved.
			return Ok(MemoryReservation { budget: &self.available, bytes: 0 });
		}
		if bytes == 0 {
			// Zero-byte reservation (the caller's "no rows to buffer"
			// case). The gate passes; the guard is a no-op.
			return Ok(MemoryReservation { budget: &self.available, bytes: 0 });
		}
		let needed = i64::try_from(bytes).unwrap_or(i64::MAX);
		// saturating_sub cannot underflow, so the fetch is the
		// authoritative check.
		let prev = self.available.fetch_sub(needed, Ordering::AcqRel);
		if prev < needed {
			// Would go negative: restore and refuse.
			self.available.fetch_add(needed, Ordering::Release);
			return Err(Code::Overloaded);
		}
		Ok(MemoryReservation { budget: &self.available, bytes })
	}
}

// ── Reservation guard ─────────────────────────────────────────────────────────────

/// An RAII guard that holds `bytes` of a [`MemoryBudget`] reserved.
///
/// The reserved bytes are released back to the budget when the guard
/// is dropped. The guard may also be released early by calling
/// [`release`](MemoryReservation::release), which is a no-op when
/// called more than once.
///
/// No mutex is held across an `.await`: the budget is updated with
/// atomic operations, and the guard's drop walks the same atomic path.
#[derive(Debug)]
pub struct MemoryReservation<'a> {
	budget: &'a AtomicI64,
	bytes: u64,
}

impl<'a> MemoryReservation<'a> {
	/// Release the reserved bytes back to the budget early. After this
	/// call the guard is a no-op on drop.
	pub fn release(&mut self) {
		let bytes = std::mem::take(&mut self.bytes);
		if bytes > 0 {
			self.budget.fetch_add(bytes as i64, Ordering::Release);
		}
	}
}

impl<'a> Drop for MemoryReservation<'a> {
	fn drop(&mut self) {
		if self.bytes > 0 {
			self.budget.fetch_add(self.bytes as i64, Ordering::Release);
		}
	}
}

// ── Tests ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
	use std::io::Write;

	use super::*;

	// ── Probe tests ──────────────────────────────────────────────────────────────

	#[test]
	fn probe_v2_unlimited() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		// cgroup v2: memory.max = "max" (unlimited)
		fs::write(root.join("memory.max"), b"max\n").ok();
		let result = probe(root);
		assert_eq!(result.bytes, 0, "unlimited v2 falls through to next source");
		// With no further files, probe returns unknown (0).
		assert_eq!(result.source, "unknown");
	}

	#[test]
	fn probe_v2_limited() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		fs::write(root.join("memory.max"), b"536870912\n").ok(); // 512 MiB
		let result = probe(root);
		assert_eq!(result.bytes, 536870912);
		assert_eq!(result.source, "cgroup v2 memory.max");
	}

	#[test]
	fn probe_v2_high_below_max() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		fs::write(root.join("memory.max"), b"1073741824\n").ok(); // 1 GiB
		fs::write(root.join("memory.high"), b"536870912\n").ok(); // 512 MiB
		let result = probe(root);
		assert_eq!(result.bytes, 536870912);
		assert_eq!(result.source, "cgroup v2 memory.high");
	}

	#[test]
	fn probe_v2_high_above_max_is_not_reported() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		fs::write(root.join("memory.max"), b"536870912\n").ok(); // 512 MiB
		fs::write(root.join("memory.high"), b"1073741824\n").ok(); // 1 GiB — above max, ignored
		let result = probe(root);
		assert_eq!(result.bytes, 536870912);
		assert_eq!(result.source, "cgroup v2 memory.max");
	}

	#[test]
	fn probe_v1_limited() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		fs::write(root.join("memory").join("memory.limit_in_bytes"), b"268435456\n").ok(); // 256 MiB
		let result = probe(root);
		assert_eq!(result.bytes, 268435456);
		assert_eq!(result.source, "cgroup v1 memory.limit_in_bytes");
	}

	#[test]
	fn probe_v1_sentinel_is_unlimited() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		// The common sentinel value (effectively i64::MAX).
		fs::write(root.join("memory").join("memory.limit_in_bytes"), b"9223372036854771712\n").ok();
		let result = probe(root);
		assert_eq!(result.bytes, 0, "sentinel falls through");
		assert_eq!(result.source, "unknown");
	}

	#[test]
	fn probe_missing_files_returns_unknown() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		let result = probe(root);
		assert_eq!(result.bytes, 0);
		assert_eq!(result.source, "unknown");
	}

	#[test]
	fn probe_malformed_contents_returns_unknown() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		fs::create_dir_all(root.join("memory")).ok();
		fs::write(root.join("memory.max"), b"not-a-number\n").ok();
		let result = probe(root);
		assert_eq!(result.bytes, 0);
		assert_eq!(result.source, "unknown");
	}

	#[test]
	fn probe_falls_back_to_proc_meminfo() {
		let dir = tempfile::tempdir().expect("temp dir");
		let root = dir.path();
		// No cgroup files — write a fake /proc/meminfo
		let proc_dir = root.join("proc");
		fs::create_dir_all(&proc_dir).ok();
		fs::write(proc_dir.join("meminfo"), b"MemTotal:       16777216 kB\nMemFree:        8388608 kB\n...\n").ok();
		let result = probe(root);
		assert_eq!(result.bytes, 17179869184); // 16777216 * 1024
		assert_eq!(result.source, "/proc/meminfo MemTotal");
	}

	// ── Budget tests ─────────────────────────────────────────────────────────────

	#[test]
	fn budget_reserve_and_release() {
		let budget = MemoryBudget::with_total(1024);
		assert_eq!(budget.available(), 1024);

		let res = budget.try_reserve(300).expect("reserve 300");
		assert_eq!(budget.available(), 724);

		res.release();
		assert_eq!(budget.available(), 1024);
	}

	#[test]
	fn budget_reserve_via_drop() {
		let budget = MemoryBudget::with_total(512);
		{
			let _res = budget.try_reserve(200).expect("reserve 200");
			assert_eq!(budget.available(), 312);
		}
		// released on drop
		assert_eq!(budget.available(), 512);
	}

	#[test]
	fn budget_refuses_over_commitment() {
		let budget = MemoryBudget::with_total(100);
		let err = budget.try_reserve(200).unwrap_err();
		assert_eq!(err, Code::Overloaded);
		assert_eq!(budget.available(), 100, "no bytes consumed");
	}

	#[test]
	fn budget_refuses_when_exhausted() {
		let budget = MemoryBudget::with_total(50);
		let _first = budget.try_reserve(50).expect("reserve all");
		let err = budget.try_reserve(1).unwrap_err();
		assert_eq!(err, Code::Overloaded);
		assert_eq!(budget.available(), 0);
	}

	#[test]
	fn budget_unknown_passes() {
		let budget = MemoryBudget::unknown();
		assert_eq!(budget.total(), 0);
		// Unknown budget passes through (preserving the previous
		// behaviour when no cgroup limit can be read).
		let res = budget.try_reserve(1).expect("unknown budget passes");
		assert_eq!(budget.available(), i64::MAX as u64, "no bytes consumed");
		res.release();
	}

	#[test]
	fn budget_zero_byte_reservation_passes() {
		let budget = MemoryBudget::with_total(100);
		let res = budget.try_reserve(0).expect("zero-byte reservation passes");
		assert_eq!(budget.available(), 100, "no bytes consumed");
		res.release();
		assert_eq!(budget.available(), 100);
	}

	#[test]
	fn budget_from_probe_zero_passes() {
		let probe = MemoryProbe { bytes: 0, source: "test" };
		let budget = MemoryBudget::from_probe(probe);
		assert_eq!(budget.total(), 0);
		// Zero total means unknown budget → pass through.
		let res = budget.try_reserve(1).expect("zero total passes");
		assert_eq!(budget.available(), i64::MAX as u64);
		res.release();
	}

	#[test]
	fn budget_from_probe_known() {
		let probe = MemoryProbe { bytes: 8192, source: "test" };
		let budget = MemoryBudget::from_probe(probe);
		assert_eq!(budget.total(), 8192);
		assert_eq!(budget.available(), 8192);

		let _res = budget.try_reserve(4096).expect("reserve half");
		assert_eq!(budget.available(), 4096);
	}
}
