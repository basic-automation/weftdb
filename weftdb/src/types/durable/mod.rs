//! Durable I/O primitives for the crash-consistency design
//! (docs/design/crash-consistency.md, slice S2).
//!
//! Every later durability slice is written against these pieces, so they carry the
//! protocol rules that are easy to get wrong in each call site:
//!
//! - [`StoreFs`] is the seam for every durable filesystem operation, with [`RealFs`]
//!   for production and, under `cfg(test)` or the `fault-injection` feature, `SimFs`
//!   (in `durable::sim`), which can produce any legal power-cut image of the store.
//! - [`write_new_durable`] writes a frame under a never-used final name with
//!   `create_new`, optionally fsyncs it through the same handle, and returns its CRC
//!   trailer. It never truncates or replaces an existing file and leaves nothing
//!   behind on error. [`WritePoints`] let the crash tests stop it between steps.
//! - [`DirSyncer`] coalesces directory fsyncs across concurrent writers, and poisons
//!   itself on the first fsync error.
//! - [`RootLock`] makes a store root single-process and names the holder when it is
//!   not.
//! - [`fault`] marks the steps of each commit protocol so the crash tests can fail,
//!   abort or pause there. Without the feature, a fault point compiles to nothing.
//! - `control_plane` proves at open that each control-plane database really runs
//!   MVCC and syncs FULL, instead of trusting the pragmas that ask for it.
//!
//! `SegmentStore::open_scoped` takes the root lock, runs the control-plane probes and
//! fsyncs the root's directories (S3). S5 onwards route their writes through the rest.

pub(crate) mod control_plane;
pub mod dirsync;
pub mod fault;
pub mod fs;
pub mod lock;
#[cfg(any(test, feature = "fault-injection"))]
pub mod sim;

pub use dirsync::DirSyncer;
pub use fault::FaultPoint;
pub use fs::{write_new_durable, FsEntry, FsMetadata, RealFs, StoreFs, SyncPolicy, WritePoints};
pub use lock::{LockHolder, RootLock, RootLockError, LOCK_FILE};
#[cfg(any(test, feature = "fault-injection"))]
pub use sim::SimFs;
