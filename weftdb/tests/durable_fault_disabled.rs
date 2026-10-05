//! Fault points are inert in a build without the `fault-injection` feature.
//!
//! An integration test links the library as production code does: without
//! `cfg(test)`. So this is the one place that can check the production shape of
//! `fault::hit`. With the feature enabled the file compiles to nothing, because the
//! armed implementation is then the one under test (in the library's unit tests).
#![cfg(not(feature = "fault-injection"))]

mod durable {
	mod fault {
		use weftdb::durable::fault::{self, FaultPoint};

		// Evaluated by the compiler, not at run time: `hit` is a `const fn` whose
		// future is zero-sized, so it cannot read an environment variable, take a lock
		// or record anything. There is nothing left for it to compile to.
		const _: () = assert!(!fault::ENABLED);
		const _: () = assert!(std::mem::size_of::<fault::Inert>() == 0);
		const PROBE: fault::Inert = fault::hit(FaultPoint::SFrameSynced);

		#[tokio::test]
		async fn hit_compiles_to_nothing_without_the_feature() {
			// Production must not be abortable through the environment.
			std::env::set_var("WEFT_FAULT", "S-frame-synced:abort,S-frame-written:err");
			assert!(fault::hit(FaultPoint::SFrameSynced).await.is_ok());
			assert!(fault::hit(FaultPoint::SFrameWritten).await.is_ok());
			assert!(PROBE.await.is_ok());
		}
	}
}
