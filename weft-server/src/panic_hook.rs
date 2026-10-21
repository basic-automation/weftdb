//! The process-wide panic hook (robustness track **ROB-2**, release plan C12, C22).
//!
//! A panic in the middle of a control-plane write is not like a panic in a read: Turso's
//! pager may be part way through a commit, its locks do not poison, and the task that
//! panics is holding a `&mut` borrow of state that the rest of the process still believes
//! is consistent. Unwinding lets the request finish as a 500 and lets the *next* request
//! answer as if nothing happened — a write that was half-issued reporting success, which is
//! the condition C22 exists to prevent. So:
//!
//! - a panic while the task is inside [`weftdb::exec::control_plane_write`] is reported on
//!   stderr and then the process **aborts**, so the exit carries a signal a supervisor sees
//!   (SIGABRT) instead of an HTTP response (FRE-6a); the restart that follows opens the
//!   store under the engine's own crash recovery, which is the component that actually
//!   knows whether a commit was torn;
//! - any other panic is reported and left unwinding — ROB-3's catch layer (wave 6) is the
//!   only layer that catches panics, and it needs unwinding to stay reachable.
//!
//! Two things this deliberately is not:
//!
//! - It does not write the poison marker (`weftdb::durable::poison_global`). A panic inside
//!   the region can precede any commit at all — most of the region reads before it writes —
//!   so the marker would turn a benign panic into an outage lasting until an operator
//!   deletes a file. Failing the process loudly is the fail-stop; judging the store is left
//!   to the open-time verification that already runs on every start.
//! - It does not catch anything, and the library still never exits a process: this hook
//!   lives in `weft-server`, the same split as the poison path, where `weftdb` reports and
//!   the host decides how the process ends.
//!
//! Install it from the binary at startup, **after** the tracing subscriber: the hook chains
//! the hook it replaces, so ordering decides who prints the panic.

use weftdb::exec::in_control_plane_write;

/// Whether [`install`] has run in this process. Startup calls `install` unconditionally, and
/// a test that installs the hook first would otherwise replace it and print the panic twice
/// through two chained hooks.
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install the ROB-2 panic hook, chaining whatever hook is already installed so the panic
/// itself is still reported with its message and location.
///
/// Idempotent: a second call does nothing, which is what lets a test install the hook
/// directly and the binary's startup path skip it.
pub fn install() {
	if INSTALLED.swap(true, std::sync::atomic::Ordering::SeqCst) {
		return;
	}
	let previous = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		// First report, in the form the operator expects: message, file, line, and the
		// span the panic happened in.
		previous(info);
		if in_control_plane_write() {
			// Written to stderr rather than traced: the panicking task may be holding the
			// pager's state, and this line has to reach whoever reads the crash log even
			// when the logging pipeline is itself what failed. One line, so a log shipper
			// that splits on newlines cannot separate the reason from the panic.
			eprintln!("weft-server: the control plane was being written when this panic happened — aborting the process: {}", shown(info));
			std::process::abort();
		}
	}));
}

/// The panic's payload and location, for the abort line.
///
/// Payload types are arbitrary and their `Display` can panic, which inside a panic hook
/// would abort the hook itself, so an unusable payload is reported as its type instead.
fn shown(info: &std::panic::PanicHookInfo<'_>) -> String {
	let payload = info.payload();
	let payload = if let Some(text) = payload.downcast_ref::<&str>() {
		(*text).to_owned()
	} else if let Some(text) = payload.downcast_ref::<String>() {
		text.clone()
	} else {
		"a panic whose payload could not be printed".to_owned()
	};
	match info.location() {
		Some(location) => format!("{payload} (at {}:{})", location.file(), location.line()),
		None => payload,
	}
}
