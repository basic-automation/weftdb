//! The disk guard (ROB-17): a write that would leave the store's filesystem below a
//! reserve is refused before any byte of it is written.
//!
//! A full disk is the one storage failure WeftDB can see coming. An `ENOSPC` half-way
//! through a frame write leaves an orphan the next open has to sweep, and the control
//! plane's own databases need room to commit the index row that would publish the
//! frame; a store that runs its filesystem to zero can find it cannot even record that
//! it stopped. So a seal asks first: [`DiskGuard::admit`] refuses a write of `need`
//! bytes unless the filesystem's available space would still hold the operator's
//! reserve afterwards, with [`StoreError::InsufficientStorage`] — `507
//! insufficient_storage` at the HTTP API.
//!
//! The same numbers are the headroom report ([`Headroom`]) that the server's headroom
//! route and its `weft_disk_*` gauges publish, so an operator sees the margin shrink
//! before the guard has to refuse anything.
//!
//! What it does not cover: maintenance outputs (compaction, reconciliation) are not
//! guarded, because a squash is how a store gets its space back; and a filesystem the
//! platform cannot describe ([`disk_space_at`] answers `None`) is admitted, the write
//! itself being the test, as for the backup headroom check (D-S16).

use std::path::Path;

use crate::StoreError;

/// What the filesystem holding a path reports about its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
	/// The filesystem's size in bytes.
	pub total_bytes: u64,
	/// The bytes an unprivileged writer can still use (`statvfs`'s `f_bavail`, not
	/// `f_bfree`: the root-reserved blocks are not WeftDB's to spend).
	pub available_bytes: u64,
}

/// The filesystem size and available bytes at `path` (or its nearest existing ancestor),
/// where the platform can say.
///
/// Unix `statvfs`. `None` where it cannot; a guard then has nothing to check against.
#[must_use]
pub fn disk_space_at(path: &Path) -> Option<DiskSpace> {
	#[cfg(unix)]
	{
		let mut probe = path.to_path_buf();
		while !probe.exists() {
			if !probe.pop() {
				return None;
			}
		}
		let c_path = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes()).ok()?;
		// SAFETY: `statvfs` is a plain C struct of integers, for which all-zero is a valid
		// value, and the call only writes into it.
		let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
		// SAFETY: `c_path` is a NUL-terminated path that outlives the call, and `stat` is a
		// valid, exclusively borrowed `statvfs`.
		if unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) } != 0 {
			return None;
		}
		// The field widths differ between platforms, hence the casts (as `free_bytes_at`).
		let fragment = stat.f_frsize as u64;
		let (blocks, available) = (stat.f_blocks as u64, stat.f_bavail as u64);
		Some(DiskSpace { total_bytes: blocks.saturating_mul(fragment), available_bytes: available.saturating_mul(fragment) })
	}
	#[cfg(not(unix))]
	{
		let _ = path;
		None
	}
}

/// The store's free-space policy: how many bytes every write must leave available.
///
/// A reserve of `0` turns the guard off, which is what a library open gets by default
/// ([`SegmentStoreOptions::default`](crate::SegmentStoreOptions::default)); the server
/// sets it from `WEFT_DISK_RESERVE_BYTES`
/// ([`SegmentStoreOptions::from_env`](crate::SegmentStoreOptions::from_env)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskGuard {
	reserve_bytes: u64,
}

/// The reserve `WEFT_DISK_RESERVE_BYTES` falls back to when unset: 256 MiB, enough for
/// the control plane to commit, checkpoint and back itself up after the frames stop.
pub const DEFAULT_DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

impl DiskGuard {
	/// A guard that admits everything.
	pub const DISABLED: Self = Self { reserve_bytes: 0 };

	/// A guard that keeps `reserve_bytes` available; `0` is [`DISABLED`](Self::DISABLED).
	#[must_use]
	pub const fn new(reserve_bytes: u64) -> Self {
		Self { reserve_bytes }
	}

	/// The reserve every write must leave available.
	#[must_use]
	pub const fn reserve_bytes(self) -> u64 {
		self.reserve_bytes
	}

	/// Whether the guard refuses anything at all.
	#[must_use]
	pub const fn is_enabled(self) -> bool {
		self.reserve_bytes > 0
	}

	/// Admit or refuse a write of `need` bytes onto a filesystem reporting `space`.
	///
	/// # Errors
	///
	/// [`StoreError::InsufficientStorage`] when the guard is enabled, the filesystem
	/// could be described, and `available < reserve + need`. A disabled guard, or a
	/// `space` of `None`, admits.
	pub fn admit(self, space: Option<DiskSpace>, need: u64) -> Result<(), StoreError> {
		let Some(space) = space.filter(|_| self.is_enabled()) else {
			return Ok(());
		};
		if space.available_bytes < self.reserve_bytes.saturating_add(need) {
			return Err(StoreError::InsufficientStorage { available_bytes: space.available_bytes, reserve_bytes: self.reserve_bytes, need_bytes: need });
		}
		Ok(())
	}

	/// The headroom `space` leaves under this guard.
	#[must_use]
	pub const fn headroom(self, space: DiskSpace) -> Headroom {
		Headroom { total_bytes: space.total_bytes, available_bytes: space.available_bytes, reserve_bytes: self.reserve_bytes, writable_bytes: space.available_bytes.saturating_sub(self.reserve_bytes) }
	}
}

/// A store's free-space margin: the filesystem's numbers and what the guard leaves of
/// them ([`SegmentStore::headroom`](crate::SegmentStore::headroom)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom {
	/// The filesystem's size in bytes.
	pub total_bytes: u64,
	/// The bytes an unprivileged writer can still use.
	pub available_bytes: u64,
	/// The reserve the guard keeps (`0`: the guard is off).
	pub reserve_bytes: u64,
	/// What a write may still take before the guard refuses it: `available - reserve`,
	/// floored at `0`.
	pub writable_bytes: u64,
}

impl Headroom {
	/// Whether the guard currently refuses every write (nothing is writable, and the
	/// guard is on).
	#[must_use]
	pub const fn refusing(&self) -> bool {
		self.reserve_bytes > 0 && self.writable_bytes == 0
	}
}

/// The reserve `WEFT_DISK_RESERVE_BYTES`'s value names: a non-negative whole number of
/// bytes (`0` turns the guard off), or [`DEFAULT_DISK_RESERVE_BYTES`] when it is unset or
/// anything else (the server's strict configuration check refuses a malformed value at
/// startup, so this leniency only reaches library embedders).
pub(crate) fn disk_reserve_bytes_from(value: Option<&str>) -> u64 {
	value.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_DISK_RESERVE_BYTES)
}

#[cfg(test)]
mod tests {
	use super::*;

	const SPACE: DiskSpace = DiskSpace { total_bytes: 10_000, available_bytes: 1_000 };

	#[test]
	fn a_disabled_guard_admits_anything() {
		assert_eq!(DiskGuard::DISABLED.admit(Some(SPACE), u64::MAX), Ok(()));
		assert_eq!(DiskGuard::new(0), DiskGuard::DISABLED);
		assert!(!DiskGuard::default().is_enabled());
	}

	#[test]
	fn the_guard_keeps_the_reserve_after_the_write() {
		let guard = DiskGuard::new(400);
		assert_eq!(guard.admit(Some(SPACE), 600), Ok(()), "exactly the reserve is left");
		assert_eq!(guard.admit(Some(SPACE), 601), Err(StoreError::InsufficientStorage { available_bytes: 1_000, reserve_bytes: 400, need_bytes: 601 }));
		assert_eq!(DiskGuard::new(u64::MAX).admit(Some(SPACE), u64::MAX), Err(StoreError::InsufficientStorage { available_bytes: 1_000, reserve_bytes: u64::MAX, need_bytes: u64::MAX }), "no overflow at the extremes");
	}

	#[test]
	fn an_undescribed_filesystem_is_admitted() {
		assert_eq!(DiskGuard::new(u64::MAX).admit(None, 1), Ok(()));
	}

	#[test]
	fn headroom_is_what_the_guard_leaves() {
		let headroom = DiskGuard::new(400).headroom(SPACE);
		assert_eq!(headroom, Headroom { total_bytes: 10_000, available_bytes: 1_000, reserve_bytes: 400, writable_bytes: 600 });
		assert!(!headroom.refusing());
		assert!(DiskGuard::new(5_000).headroom(SPACE).refusing(), "a reserve above what is available refuses every write");
		assert!(!DiskGuard::DISABLED.headroom(DiskSpace { total_bytes: 1, available_bytes: 0 }).refusing(), "a disabled guard never refuses");
	}

	#[test]
	fn the_reserve_value_parses_leniently() {
		assert_eq!(disk_reserve_bytes_from(None), DEFAULT_DISK_RESERVE_BYTES);
		assert_eq!(disk_reserve_bytes_from(Some(" 0 ")), 0);
		assert_eq!(disk_reserve_bytes_from(Some("1048576")), 1_048_576);
		assert_eq!(disk_reserve_bytes_from(Some("-1")), DEFAULT_DISK_RESERVE_BYTES);
		assert_eq!(disk_reserve_bytes_from(Some("lots")), DEFAULT_DISK_RESERVE_BYTES);
	}

	#[cfg(unix)]
	#[test]
	fn the_filesystem_holding_a_missing_path_is_described() {
		let dir = tempfile::tempdir().unwrap();
		let space = disk_space_at(&dir.path().join("not/yet/here")).expect("the nearest existing ancestor is described");
		assert!(space.total_bytes >= space.available_bytes);
		assert!(space.total_bytes > 0);
	}
}
