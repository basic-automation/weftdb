//! The seal's index write is spanned and counted (roadmap Phase 3): sealing a segment closes a
//! `control_plane.index.insert` span whose `rows_changed` is the one row the insert changed.

use std::{
	io, str::FromStr, sync::{Arc, Mutex}
};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use tracing_subscriber::fmt::{format::FmtSpan, MakeWriter};
use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
use weftdb::SegmentStore;

/// Every `fmt` line the subscriber writes, read back from a shared buffer.
#[derive(Clone, Default)]
struct SpanLog(Arc<Mutex<Vec<u8>>>);

impl io::Write for SpanLog {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.lock().expect("log buffer").extend_from_slice(buf);
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl<'a> MakeWriter<'a> for SpanLog {
	type Writer = Self;

	fn make_writer(&'a self) -> Self::Writer {
		self.clone()
	}
}

fn bd(text: &str) -> BigDecimal {
	BigDecimal::from_str(text).expect("decimal literal")
}

#[tokio::test]
async fn sealing_closes_an_index_insert_span_with_one_row_changed() {
	let dir = TempDir::new().expect("temp dir");
	let log = SpanLog::default();
	let subscriber = tracing_subscriber::fmt().with_writer(log.clone()).with_ansi(false).with_span_events(FmtSpan::CLOSE).with_max_level(tracing::Level::DEBUG).finish();
	let _guard = tracing::subscriber::set_default(subscriber);

	let store = SegmentStore::open(dir.path()).await.expect("opens store");
	store.declare("price", &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares aspect");
	store.seal_declared("price", &[100_i64, 110, 120], &[bd("1.5"), bd("2.5"), bd("3.5")]).await.expect("seals");

	let text = String::from_utf8(log.0.lock().expect("log buffer").clone()).expect("utf-8 log");
	assert!(text.lines().any(|line| line.contains("control_plane.index.insert") && line.contains("rows_changed=1")), "the seal closed no index insert span with one row changed:\n{text}");
}
