//! Per-aspect commit and maintenance locks, and the id allocator the commit lock guards
//! (docs/design/crash-consistency.md section 5.1, `AspectLocks`; slice S7).
//!
//! A [`SegmentStore`](crate::SegmentStore) keeps two locks per aspect:
//!
//! - **commit** guards the aspect's [`AspectState`] (its id allocator) and is held around
//!   each control-plane transaction of the aspect: a seal's insert and rollup fold, a
//!   maintenance pass's row rewrites and member deletes, a rollup rebuild. Writers of one
//!   aspect therefore never race each other's rows (the allocator row, the rollup row), so
//!   their transactions do not conflict, and a rollup is never folded or rebuilt from a
//!   read another writer has made stale. It is held for one transaction at a time, never
//!   across frame I/O, so seals of one aspect still encode and write their frames in
//!   parallel.
//! - **maint** is held for a whole maintenance operation (a reconcile, a split, an overlap
//!   merge, a squash, a compaction), so two of them never interleave on one aspect: a
//!   reconcile that read a segment could otherwise write it back after a squash had merged
//!   it away.
//!
//! The allocator hands out ids that are never reissued, so a seal never takes the id (and
//! with it the `{aspect}-{id}` frame name) of a segment that was deleted, or of a frame a
//! crashed seal left behind. Its state is seeded the first time an aspect's commit lock is
//! taken, from the largest of the persisted `aspect_seq.next_id`, `MAX(id) + 1` over the
//! aspect's rows, and one past the highest id a legacy-named file in `segments/` carries,
//! and every commit that uses an id raises `aspect_seq.next_id` past it in the same
//! transaction.
//!
//! The locks are in-process: a store root has one owner (its `LOCK`), so nothing else
//! writes its index.

use std::{
	collections::HashMap, future::Future, sync::{Arc, Mutex as StdMutex, PoisonError}, time::Duration
};

use tokio::sync::{Mutex, OwnedMutexGuard};

/// What an aspect's commit lock guards: its id allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AspectState {
	/// The next segment id to hand out. Every id below it was handed out once.
	pub next_id: u64,
	/// The aspect's epoch, as `aspect_seq` records it. Nothing advances it yet (the
	/// write-once swaps and seals of S8-S10 do); a commit writes it back unchanged.
	pub epoch: u64,
}

/// One aspect's two locks.
#[derive(Default)]
struct AspectLock {
	/// `None` until the allocator is seeded, which happens under the lock.
	commit: Arc<Mutex<Option<AspectState>>>,
	maint: Arc<Mutex<()>>,
}

/// The per-aspect locks of one store, created on first use and kept for the store's life.
#[derive(Default)]
pub struct AspectLocks {
	aspects: StdMutex<HashMap<String, Arc<AspectLock>>>,
}

impl AspectLocks {
	/// `aspect`'s locks, created on first use.
	fn of(&self, aspect: &str) -> Arc<AspectLock> {
		let mut aspects = self.aspects.lock().unwrap_or_else(PoisonError::into_inner);
		aspects.entry(aspect.to_string()).or_default().clone()
	}

	/// Take `aspect`'s commit lock, seeding its allocator with `seed` if this is the
	/// first time. A failed seed leaves the allocator unseeded, for the next taker to try.
	///
	/// # Errors
	///
	/// The seed's error.
	pub async fn commit<Seed>(&self, aspect: &str, seed: impl FnOnce() -> Seed) -> anyhow::Result<CommitGuard>
	where
		Seed: Future<Output = anyhow::Result<AspectState>>,
	{
		let mut slot = self.of(aspect).commit.clone().lock_owned().await;
		let state = if let Some(state) = *slot {
			state
		} else {
			let state = seed().await?;
			*slot = Some(state);
			state
		};
		Ok(CommitGuard { slot, state })
	}

	/// Take `aspect`'s maintenance lock, waiting at most `wait`; `None` when another
	/// operation still holds it then.
	pub async fn maint_within(&self, aspect: &str, wait: Duration) -> Option<MaintGuard> {
		let lock = self.of(aspect).maint.clone();
		// `timeout` polls the lock before its timer, so a free lock is taken even when
		// `wait` is zero. Dropping a waiting `lock_owned` future gives up its place in the
		// queue and nothing else.
		let held = tokio::time::timeout(wait, lock.lock_owned()).await.ok()?;
		Some(MaintGuard { aspect: aspect.to_string(), _held: held })
	}

	/// Take `aspect`'s maintenance lock if it is free.
	pub fn try_maint(&self, aspect: &str) -> Option<MaintGuard> {
		let held = self.of(aspect).maint.clone().try_lock_owned().ok()?;
		Some(MaintGuard { aspect: aspect.to_string(), _held: held })
	}
}

/// An aspect's commit lock, held, with its seeded allocator.
pub struct CommitGuard {
	slot: OwnedMutexGuard<Option<AspectState>>,
	state: AspectState,
}

impl CommitGuard {
	/// The allocator as it stands.
	pub const fn state(&self) -> AspectState {
		self.state
	}

	/// Hand out the next id. It is never handed out again, whether or not the caller
	/// commits it.
	///
	/// # Errors
	///
	/// When the aspect has used up every id.
	pub fn allocate(&mut self) -> anyhow::Result<u64> {
		let id = self.state.next_id;
		self.state.next_id = id.checked_add(1).ok_or_else(|| anyhow::anyhow!("the aspect has used every segment id"))?;
		*self.slot = Some(self.state);
		Ok(id)
	}
}

/// An aspect's maintenance lock, held for one maintenance operation, naming the aspect it
/// was taken for so that the operation cannot work on another one.
pub struct MaintGuard {
	aspect: String,
	_held: OwnedMutexGuard<()>,
}

impl MaintGuard {
	/// The aspect this guard holds.
	pub fn aspect(&self) -> &str {
		&self.aspect
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn seeded(next_id: u64) -> impl FnOnce() -> std::future::Ready<anyhow::Result<AspectState>> {
		move || std::future::ready(Ok(AspectState { next_id, epoch: 0 }))
	}

	/// The allocator is seeded once, under the lock, and keeps what it handed out: a later
	/// taker's seed is never consulted, and a failed seed is retried by the next taker.
	#[tokio::test]
	async fn the_allocator_is_seeded_once_and_never_hands_an_id_out_twice() {
		let locks = AspectLocks::default();
		let failed = locks.commit("a", || std::future::ready(Err(anyhow::anyhow!("the seed failed")))).await.err().map(|e| e.to_string());
		let mut first = locks.commit("a", seeded(5)).await.expect("seeds");
		let ids = [first.allocate().expect("allocates"), first.allocate().expect("allocates")];
		drop(first);
		let mut again = locks.commit("a", seeded(0)).await.expect("takes the lock again");
		let next = again.allocate().expect("allocates");
		drop(again);
		let mut other = locks.commit("b", seeded(0)).await.expect("seeds another aspect");
		let other_id = other.allocate().expect("allocates");
		drop(other);
		assert_eq!(failed.as_deref(), Some("the seed failed"));
		assert_eq!(ids, [5, 6]);
		assert_eq!(next, 7, "the second taker's seed is not used");
		assert_eq!(other_id, 0, "aspects allocate independently");
	}

	/// The commit lock serializes its takers per aspect, and only per aspect.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn the_commit_lock_is_per_aspect() {
		let locks = Arc::new(AspectLocks::default());
		let held = locks.commit("a", seeded(0)).await.expect("seeds");
		let other = tokio::time::timeout(Duration::from_secs(10), locks.commit("b", seeded(0))).await.expect("another aspect's lock is free");
		let waiter = tokio::spawn({
			let locks = locks.clone();
			async move { locks.commit("a", seeded(0)).await.map(|guard| guard.state()) }
		});
		tokio::time::sleep(Duration::from_millis(50)).await;
		let waited = !waiter.is_finished();
		drop(held);
		let state = waiter.await.expect("joins").expect("takes the lock once it is free");
		drop(other);
		assert!(waited, "a second taker of one aspect's lock waits");
		assert_eq!(state.next_id, 0);
	}

	/// The maintenance lock is taken at once when it is free, refused by `try_maint` and
	/// by an expired wait while it is held, and taken by a wait it is released within.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn the_maintenance_lock_waits_at_most_as_long_as_asked() {
		let locks = Arc::new(AspectLocks::default());
		let held = locks.maint_within("a", Duration::ZERO).await.expect("a free lock is taken even with no wait");
		let tried = locks.try_maint("a").is_some();
		let expired = locks.maint_within("a", Duration::from_millis(50)).await.is_some();
		let other = locks.try_maint("b").map(|guard| guard.aspect().to_string());
		let waiter = tokio::spawn({
			let locks = locks.clone();
			async move { locks.maint_within("a", Duration::from_secs(30)).await.map(|guard| guard.aspect().to_string()) }
		});
		tokio::time::sleep(Duration::from_millis(50)).await;
		drop(held);
		let waited = waiter.await.expect("joins");
		assert!(!tried, "try_maint does not take a held lock");
		assert!(!expired, "a wait that runs out does not take it");
		assert_eq!(other.as_deref(), Some("b"), "another aspect's lock is free");
		assert_eq!(waited.as_deref(), Some("a"), "a wait the lock is released within takes it");
	}
}
