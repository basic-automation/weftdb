//! The Daemons registry and daemon supervision (robustness track **ROB-3**,
//! release plan C12 and §5 D-S14).
//!
//! Before ROB-3 the server's background tasks — the reconcile daemon, the
//! backup daemon, the one-shot GPU calibration — were spawned detached and
//! their handles dropped on purpose. That shape has a hole a supervisor
//! exists to close: a daemon that *panics* dies silently. Its loop is gone,
//! nothing restarts it, nothing records that it ever failed, and the server
//! keeps serving with a maintenance behaviour the operator asked for simply
//! absent. (A daemon that returns an error from a tick already logs and
//! continues inside its own loop; a panic is the failure that loop cannot
//! report about itself.)
//!
//! [`Daemons`] is the registry every background task is spawned through
//! instead:
//!
//! - **Supervision.** A supervised daemon ([`Daemons::spawn`]) whose task
//!   ends in a panic (`JoinError::is_panic`) is restarted from its factory,
//!   after [`RESTART_DELAY`], for as long as the registry is not shut down.
//!   The restart count is recorded in the daemon's snapshot. A daemon that
//!   finishes on its own is *not* restarted — it said it was done — and a
//!   daemon cancelled by the registry's shutdown is a clean stop, not a
//!   failure. One-shot work ([`Daemons::spawn_once`], e.g. GPU calibration)
//!   is tracked the same way but never restarted: a calibration that panics
//!   leaves the defaults in force, exactly as it did before the registry.
//! - **The shared shutdown watch.** The registry owns a
//!   [`tokio::sync::watch`] shutdown flag. Every supervised task selects on
//!   it, and [`Daemons::shutdown_watch`] hands out receivers — the watch
//!   D-S14's amendment (plan §5) names as shared with this registry, so the
//!   ingest wiring and the daemons stop on the same signal rather than on
//!   two watches that can disagree. [`Daemons::shutdown`] raises it: no
//!   daemon is restarted after it, and every running daemon task is
//!   aborted, which is its stop.
//! - **Snapshots.** [`Daemons::snapshot`] reports every registered daemon's
//!   name, state and restart count, in registration order — the record the
//!   old drop-the-handle shape could not produce.
//!
//! The registry never fails startup and never panics: a factory that
//! panics before returning its future is caught by the same supervision as
//! a future that panics after returning, because the factory is called
//! inside the supervised task.

use std::{
	future::Future, sync::{Arc, Mutex}, time::Duration
};

use tokio::{sync::watch, task::JoinHandle};

/// How long supervision waits before restarting a panicked daemon: long
/// enough that a daemon panicking in a tight loop cannot spin the CPU,
/// short enough that a maintenance daemon is back within an operator's
/// glance at the log.
pub const RESTART_DELAY: Duration = Duration::from_millis(100);

/// Where a registered daemon is in its supervised life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
	/// The daemon's task is running (or its supervisor is between the
	/// factory call and the task's first poll).
	Running,
	/// The daemon panicked and supervision is waiting out
	/// [`RESTART_DELAY`] before restarting it.
	Restarting,
	/// The daemon is not running and will not be restarted: it finished on
	/// its own, it was a one-shot, or the registry shut down.
	Stopped,
}

/// One daemon's record in a [`Daemons::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSnapshot {
	/// The name the daemon was registered under (`reconcile`, `backup`, …).
	pub name: String,
	/// Its current supervised state.
	pub state: DaemonState,
	/// How many times supervision has restarted it after a panic.
	pub restarts: u32,
}

/// The mutable half of one registry entry, behind the registry's mutex.
#[derive(Debug)]
struct Entry {
	name: String,
	state: DaemonState,
	restarts: u32,
}

/// The Daemons registry: spawn background work through it, read it back
/// with [`snapshot`](Self::snapshot), stop everything with
/// [`shutdown`](Self::shutdown). Cloneable — every clone is the same
/// registry, which is what lets startup phases register into one.
#[derive(Debug, Clone)]
pub struct Daemons {
	entries: Arc<Mutex<Vec<Entry>>>,
	shutdown_tx: watch::Sender<bool>,
}

impl Default for Daemons {
	fn default() -> Self {
		Self::new()
	}
}

impl Daemons {
	/// A fresh, empty registry, not shut down.
	#[must_use]
	pub fn new() -> Self {
		let (shutdown_tx, _) = watch::channel(false);
		Self { entries: Arc::new(Mutex::new(Vec::new())), shutdown_tx }
	}

	/// A receiver on the registry's shared shutdown watch (plan §5, D-S14):
	/// it reads `true` once [`shutdown`](Self::shutdown) has run. Hand one
	/// to anything that must stop when the daemons stop.
	#[must_use]
	pub fn shutdown_watch(&self) -> watch::Receiver<bool> {
		self.shutdown_tx.subscribe()
	}

	/// Whether [`shutdown`](Self::shutdown) has run.
	#[must_use]
	pub fn is_shutdown(&self) -> bool {
		*self.shutdown_tx.borrow()
	}

	/// Raise the shutdown watch: supervised daemons are aborted and none
	/// is restarted. Idempotent, and safe to call while daemons run.
	pub fn shutdown(&self) {
		let _ = self.shutdown_tx.send(true);
	}

	/// Every registered daemon's snapshot, in registration order.
	#[must_use]
	pub fn snapshot(&self) -> Vec<DaemonSnapshot> {
		self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().map(|entry| DaemonSnapshot { name: entry.name.clone(), state: entry.state, restarts: entry.restarts }).collect()
	}

	/// Register and start a **supervised** daemon: `make` is a factory
	/// producing the daemon's future, called once at start and again after
	/// every panic, until the registry shuts down. Returns the supervisor's
	/// handle; dropping it does not stop the daemon (the supervisor owns
	/// the daemon task), but the registry's [`shutdown`](Self::shutdown)
	/// does.
	#[must_use = "the supervisor's handle is the caller's only way to join the supervision loop"]
	pub fn spawn<F, Fut>(&self, name: impl Into<String>, make: F) -> JoinHandle<()>
	where
		F: FnMut() -> Fut + Send + 'static,
		Fut: Future<Output = ()> + Send + 'static,
	{
		self.supervise(name.into(), make, true)
	}

	/// Register and start a **one-shot** task: tracked in the registry and
	/// stopped by [`shutdown`](Self::shutdown), but never restarted — a
	/// panic is recorded in its restart count's place by the state moving
	/// to [`DaemonState::Stopped`], and the task stays down.
	///
	/// # Panics
	///
	/// Never in practice: the one-shot factory's `expect` is unreachable,
	/// because supervision calls the factory exactly once when restarts
	/// are disabled — the branch that would call it again is the restart
	/// branch `spawn_once` opts out of.
	#[must_use = "the supervisor's handle is the caller's only way to join the supervision loop"]
	pub fn spawn_once<Fut>(&self, name: impl Into<String>, fut: Fut) -> JoinHandle<()>
	where
		Fut: Future<Output = ()> + Send + 'static,
	{
		let mut once = Some(fut);
		self.supervise(name.into(), move || once.take().expect("a one-shot factory is called once"), false)
	}

	/// The supervision loop behind [`spawn`](Self::spawn) and
	/// [`spawn_once`](Self::spawn_once).
	fn supervise<F, Fut>(&self, name: String, mut make: F, restart_on_panic: bool) -> JoinHandle<()>
	where
		F: FnMut() -> Fut + Send + 'static,
		Fut: Future<Output = ()> + Send + 'static,
	{
		let index = {
			let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			entries.push(Entry { name: name.clone(), state: DaemonState::Running, restarts: 0 });
			entries.len() - 1
		};
		let entries = Arc::clone(&self.entries);
		let mut shutdown_rx = self.shutdown_tx.subscribe();
		tokio::spawn(async move {
			loop {
				if *shutdown_rx.borrow() {
					set_state(&entries, index, DaemonState::Stopped);
					break;
				}
				set_state(&entries, index, DaemonState::Running);
				let mut daemon = tokio::spawn(make());
				let outcome = tokio::select! {
					changed = shutdown_rx.changed() => {
						let _ = changed;
						daemon.abort();
						let _ = (&mut daemon).await;
						None
					},
					joined = &mut daemon => Some(joined),
				};
				let Some(joined) = outcome else {
					set_state(&entries, index, DaemonState::Stopped);
					break;
				};
				match joined {
					Ok(()) => {
						set_state(&entries, index, DaemonState::Stopped);
						break;
					}
					Err(err) if err.is_panic() && restart_on_panic && !*shutdown_rx.borrow() => {
						let restarts = bump_restarts(&entries, index);
						eprintln!("daemon {name}: panicked ({}); supervision restarts it (restart {restarts})", weftdb::exec::panic_message(&*err.into_panic()));
						// The restart delay is itself shutdown-aware: a
						// shutdown during the wait is a stop, not a restart.
						if tokio::time::timeout(RESTART_DELAY, shutdown_rx.changed()).await.is_ok() {
							set_state(&entries, index, DaemonState::Stopped);
							break;
						}
					}
					Err(err) => {
						if err.is_panic() {
							eprintln!("daemon {name}: panicked ({}); it is not restarted", weftdb::exec::panic_message(&*err.into_panic()));
						}
						set_state(&entries, index, DaemonState::Stopped);
						break;
					}
				}
			}
		})
	}
}

/// Set one entry's state, tolerating a poisoned registry mutex (the state
/// is plain data; a poisoned lock here means another supervisor panicked
/// while holding it, which is no reason to lose this update too).
fn set_state(entries: &Mutex<Vec<Entry>>, index: usize, state: DaemonState) {
	let mut entries = entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
	if let Some(entry) = entries.get_mut(index) {
		entry.state = state;
	}
}

/// Mark one entry as restarting and return its new restart count.
fn bump_restarts(entries: &Mutex<Vec<Entry>>, index: usize) -> u32 {
	let mut entries = entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
	entries.get_mut(index).map_or(0, |entry| {
		entry.restarts += 1;
		entry.state = DaemonState::Restarting;
		entry.restarts
	})
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicU32, Ordering};

	use super::*;

	/// Poll a snapshot until `name` satisfies `want`, or give up.
	async fn wait_for(daemons: &Daemons, name: &str, want: impl Fn(&DaemonSnapshot) -> bool) -> DaemonSnapshot {
		for _ in 0..200 {
			if let Some(found) = daemons.snapshot().into_iter().find(|snap| snap.name == name && want(snap)) {
				return found;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		panic!("daemon {name} never reached the wanted state; snapshot: {:?}", daemons.snapshot());
	}

	#[tokio::test]
	async fn a_panicking_daemon_is_restarted_and_the_restart_is_recorded() {
		let daemons = Daemons::new();
		let attempts = Arc::new(AtomicU32::new(0));
		let factory_attempts = Arc::clone(&attempts);
		drop(daemons.spawn("flaky", move || {
			let attempts = Arc::clone(&factory_attempts);
			async move {
				// Panic on the first two runs, then run until shutdown.
				if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
					panic!("tick blew up");
				}
				std::future::pending::<()>().await;
			}
		}));
		let snap = wait_for(&daemons, "flaky", |snap| snap.restarts == 2 && snap.state == DaemonState::Running).await;
		assert_eq!(snap.restarts, 2);
		assert_eq!(attempts.load(Ordering::SeqCst), 3, "the factory ran once per attempt");
		daemons.shutdown();
		wait_for(&daemons, "flaky", |snap| snap.state == DaemonState::Stopped).await;
	}

	#[tokio::test]
	async fn a_daemon_that_finishes_is_stopped_not_restarted() {
		let daemons = Daemons::new();
		let runs = Arc::new(AtomicU32::new(0));
		let factory_runs = Arc::clone(&runs);
		drop(daemons.spawn("short", move || {
			let runs = Arc::clone(&factory_runs);
			async move {
				runs.fetch_add(1, Ordering::SeqCst);
			}
		}));
		let snap = wait_for(&daemons, "short", |snap| snap.state == DaemonState::Stopped).await;
		assert_eq!(snap.restarts, 0);
		assert_eq!(runs.load(Ordering::SeqCst), 1, "a finished daemon is not run again");
	}

	#[tokio::test]
	async fn a_one_shot_panic_is_recorded_as_stopped_and_never_restarted() {
		let daemons = Daemons::new();
		drop(daemons.spawn_once("once", async { panic!("one-shot blew up") }));
		let snap = wait_for(&daemons, "once", |snap| snap.state == DaemonState::Stopped).await;
		assert_eq!(snap.restarts, 0);
	}

	#[tokio::test]
	async fn shutdown_stops_a_running_daemon_and_raises_the_shared_watch() {
		let daemons = Daemons::new();
		let mut watch = daemons.shutdown_watch();
		assert!(!*watch.borrow(), "the watch starts unraised");
		drop(daemons.spawn("long", || async { std::future::pending::<()>().await }));
		wait_for(&daemons, "long", |snap| snap.state == DaemonState::Running).await;
		daemons.shutdown();
		assert!(daemons.is_shutdown());
		watch.changed().await.expect("the watch fires");
		assert!(*watch.borrow());
		wait_for(&daemons, "long", |snap| snap.state == DaemonState::Stopped).await;
	}

	#[tokio::test]
	async fn snapshot_lists_daemons_in_registration_order() {
		let daemons = Daemons::new();
		drop(daemons.spawn_once("first", async { std::future::pending::<()>().await }));
		drop(daemons.spawn_once("second", async { std::future::pending::<()>().await }));
		let names: Vec<String> = daemons.snapshot().into_iter().map(|snap| snap.name).collect();
		assert_eq!(names, ["first", "second"]);
		daemons.shutdown();
	}
}
