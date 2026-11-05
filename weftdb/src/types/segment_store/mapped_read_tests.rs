//! Roadmap Phase 5, memory-mapped GPU I/O: a range read and an interpolation with
//! [`SegmentStoreOptions::mmap_reads`] on return exactly what the buffered read returns.
//! Run with `--features mmap-reads` these exercise the mapping itself; without the feature
//! the option is accepted and the read stays buffered, so they still hold.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use splimes::{Interpolator, Resolution, Spline};
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{SegmentStore, SegmentStoreOptions};

const ASPECT: &str = "wave";

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

/// Seal three segments of an irregular, non-linear series into a fresh store at `root`:
/// timestamps `3·i + (i mod 2)` seconds, values `(i² mod 97) / 4`, 400 rows per segment.
async fn seed(root: &Path) {
	let store = SegmentStore::open(root).await.expect("opens");
	store.declare(ASPECT, &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
	for part in 0_i64..3 {
		let ts: Vec<i64> = (part * 400..(part + 1) * 400).map(|i| 3 * i + (i % 2)).collect();
		let vs: Vec<BigDecimal> = (part * 400..(part + 1) * 400).map(|i| bd(&format!("{}.{}", (i * i % 97) / 4, (i * i % 97) % 4 * 25))).collect();
		store.seal_declared(ASPECT, &ts, &vs).await.expect("seals");
	}
}

async fn open(root: &Path, mmap_reads: bool) -> SegmentStore {
	SegmentStore::open_with_options(root, SegmentStoreOptions::default().with_mmap_reads(mmap_reads)).await.expect("reopens")
}

#[test]
fn mapped_reads_are_off_by_default() {
	assert!(!SegmentStoreOptions::default().mmap_reads, "opt-in: the buffered read is the default");
	assert!(SegmentStoreOptions::default().with_mmap_reads(true).mmap_reads);
}

#[tokio::test]
async fn a_mapped_range_read_returns_the_buffered_rows() {
	let dir = TempDir::new().expect("tempdir");
	seed(dir.path()).await;
	// Windows: inside one segment, across a segment boundary, all three, and an empty one.
	let windows = [(100_i64, 600_i64), (1100, 1300), (0, 3700), (5000, 6000)];
	let mut buffered = Vec::new();
	{
		let store = open(dir.path(), false).await;
		for (start, end) in windows {
			buffered.push(store.read_time_range(ASPECT, start, end).await.expect("buffered read"));
		}
	}
	let store = open(dir.path(), true).await;
	for ((start, end), expected) in windows.into_iter().zip(&buffered) {
		let mapped = store.read_time_range(ASPECT, start, end).await.expect("mapped read");
		assert_eq!(&mapped, expected, "[{start}, {end}]: the mapped read returns the buffered rows");
	}
	assert!(!buffered[2].0.is_empty() && buffered[3].0.is_empty(), "the windows read what they were chosen to");
}

#[tokio::test]
async fn a_mapped_interpolation_is_the_buffered_one_bit_for_bit() {
	let dir = TempDir::new().expect("tempdir");
	seed(dir.path()).await;
	let cubic = Interpolator::new(Spline::Cubic, Resolution::Seconds);
	let bits = |series: &splimes::Interpolation<f64>| series.values().iter().copied().map(f64::to_bits).collect::<Vec<u64>>();
	let (expected, expected_kinds) = {
		let store = open(dir.path(), false).await;
		let series = store.interpolate_range_f64(ASPECT, 1000, 1400, 30, cubic).await.expect("buffered interpolation");
		(bits(&series), series.kinds().to_vec())
	};
	let store = open(dir.path(), true).await;
	let series = store.interpolate_range_f64(ASPECT, 1000, 1400, 30, cubic).await.expect("mapped interpolation");
	assert_eq!(series.len(), 401, "one point per second of [1000, 1400]");
	assert_eq!(bits(&series), expected, "same knots in, same values out");
	assert_eq!(series.kinds(), expected_kinds.as_slice(), "and the same provenance");
}

#[tokio::test]
async fn a_mapped_read_of_a_missing_frame_is_an_error_not_a_crash() {
	let dir = TempDir::new().expect("tempdir");
	seed(dir.path()).await;
	let store = open(dir.path(), true).await;
	for entry in std::fs::read_dir(dir.path().join("segments")).expect("lists segments/") {
		let path = entry.expect("an entry").path();
		if path.extension().is_some_and(|ext| ext == "weftseg") {
			std::fs::remove_file(&path).expect("removes the frame");
		}
	}
	assert!(store.read_time_range(ASPECT, 0, 3700).await.is_err(), "an indexed frame that is gone fails the read");
}
