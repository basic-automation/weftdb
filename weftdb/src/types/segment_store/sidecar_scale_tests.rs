//! A sidecar-served downsample carries the column's decimal scale (INT-0 review,
//! 2026-10-08): on a `ScaledI64` aspect a zero `min`, `max`, `sum`, `avg`, `first` or
//! `last` must come back as `(0, scale)`, the representation `read_time_range` + `reduce`
//! gives it, not as `(0, 0)`, and on a `Decimal128` one every value keeps its own scale.
//! `BigDecimal` equality ignores the scale, so every comparison here is on
//! `as_bigint_and_exponent`.

use std::str::FromStr;

use bigdecimal::{num_bigint::BigInt, BigDecimal, Zero};
use chrono::DateTime;
use splimes::{Point, Resolution};
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType, SegmentDescriptor};
use weft_reduce::{reduce, reduce_partial, Bucket};

use super::SegmentStore;
use crate::{PartialSidecar, PartialSidecarPolicy, SIDECAR_AGGREGATIONS};

const ASPECT: &str = "flow";

/// Sidecars at a minutes base with an hours rollup tier, for every segment.
const BASE: Resolution = Resolution::Minutes;
const TIERS: [Resolution; 1] = [Resolution::Hours];

/// The mantissas of one six-row minute bucket: all zero, a zero `min`/`first`/`last`, a
/// zero `max`, a zero `sum`/`avg`/`last`, and a zero `min` under a non-zero sum.
const PATTERNS: [[i64; 6]; 5] = [[0, 0, 0, 0, 0, 0], [0, 1, 2, 0, 3, 0], [-3, 0, -1, -2, 0, -1], [-2, 2, -5, 5, 0, 0], [7, 0, 0, 0, 0, 9]];

/// One segment of the fixture: ten minute buckets of six rows ten seconds apart from
/// `start`, bucket `k` taking `pattern(k)`.
struct Seg {
	start: i64,
	pattern: fn(usize) -> usize,
}

/// Overlapping and disjoint segments, in order: `a` and `b` share hour 0; `z`, all zeros,
/// interleaves with `a` five seconds off, so their minute buckets merge; `c`, all zeros,
/// sits alone in hour 2; and `d`, every bucket summing to zero, alone in hour 3.
const SEGMENTS: [Seg; 5] = [Seg { start: 0, pattern: |k| k % 5 }, Seg { start: 1_200, pattern: |k| (k + 2) % 5 }, Seg { start: 5, pattern: |_| 0 }, Seg { start: 7_200, pattern: |_| 0 }, Seg { start: 10_800, pattern: |_| 3 }];

fn rows(seg: &Seg, scale: i64) -> (Vec<i64>, Vec<BigDecimal>) {
	let mut ts = Vec::new();
	let mut vs = Vec::new();
	for k in 0..10 {
		for (j, &m) in PATTERNS[(seg.pattern)(k)].iter().enumerate() {
			ts.push(seg.start + (k as i64) * 60 + (j as i64) * 10);
			vs.push(BigDecimal::new(BigInt::from(m), scale));
		}
	}
	(ts, vs)
}

/// The queries compared: the whole history (the merge path), and the windows of `c` and
/// `d` alone (the single-segment path), each at the base, the rollup tier, and a
/// resolution re-keyed from that tier.
fn queries() -> Vec<(i64, i64, Resolution)> {
	let windows = [(i64::MIN, i64::MAX), (7_200, 7_200 + 599), (10_800, 10_800 + 599)];
	windows.into_iter().flat_map(|(s, e)| [Resolution::Minutes, Resolution::Hours, Resolution::Days].map(|r| (s, e, r))).collect()
}

/// `read_time_range` followed by one `reduce`, for each query.
async fn decode_then_reduce(store: &SegmentStore) -> Vec<Vec<Bucket>> {
	let mut out = Vec::new();
	for (start, end, resolution) in queries() {
		let (ts, vs) = store.read_time_range(ASPECT, start, end).await.expect("reads");
		let points: Vec<Point> = ts.iter().zip(vs).filter_map(|(t, v)| v.map(|v| Point::new(DateTime::from_timestamp(*t, 0).expect("instant"), v))).collect();
		out.push(reduce(&points, resolution, None, None, &SIDECAR_AGGREGATIONS).expect("reduces"));
	}
	out
}

/// `downsample_range` for each query must equal `expected` value for value, and digit
/// for digit and exponent for exponent.
async fn assert_matches(store: &SegmentStore, expected: &[Vec<Bucket>], label: &str) {
	let mut mismatches = Vec::new();
	for ((start, end, resolution), single) in queries().into_iter().zip(expected) {
		let served = store.downsample_range(ASPECT, start, end, resolution, &SIDECAR_AGGREGATIONS).await.expect("downsamples");
		assert_eq!(&served, single, "{label}: [{start}, {end}] at {resolution:?}");
		for (s, d) in served.iter().zip(single) {
			for (name, value) in &d.values {
				let got = s.values.get(name).map(BigDecimal::as_bigint_and_exponent);
				if got.as_ref() != Some(&value.as_bigint_and_exponent()) {
					mismatches.push(format!("{name} at {} for [{start}, {end}] at {resolution:?}: {got:?}, decode-then-reduce {:?}", d.timestamp, value.as_bigint_and_exponent()));
				}
			}
		}
	}
	assert!(mismatches.is_empty(), "{label}: {} values differ in digits or exponent:\n{}", mismatches.len(), mismatches.join("\n"));
}

/// Seal the fixture at `scale` into a fresh store of `physical` columns writing sidecars,
/// and return it with its descriptors and the decode-then-reduce answers, taken while the
/// frames exist.
async fn fixture(dir: &TempDir, physical: PhysicalType, scale: i64) -> (SegmentStore, Vec<SegmentDescriptor>, Vec<Vec<Bucket>>) {
	let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at_tiered(BASE, 1, &TIERS));
	let schema = AspectSchema::new(physical, BigDecimal::from_str("0").expect("parses"), TimeUnit::Seconds);
	store.declare(ASPECT, &schema).await.expect("declares");
	let mut descriptors = Vec::new();
	for seg in &SEGMENTS {
		let (ts, vs) = rows(seg, scale);
		descriptors.push(store.seal(ASPECT, &schema, &ts, &vs).await.expect("seals"));
	}
	for descriptor in &descriptors {
		assert!(store.load_partial_sidecar(ASPECT, descriptor).await.expect("loads").is_some(), "segment {} has a sidecar", descriptor.id);
	}
	let expected = decode_then_reduce(&store).await;
	// The fixture has zeros to lose: at least one zero in each streaming reduction. (A sum
	// starts at scale 0, so under a negative scale only the others carry it.)
	let names: &[&str] = if scale > 0 { &["min", "max", "sum", "avg", "first", "last"] } else { &["min", "max", "first", "last"] };
	for name in names {
		assert!(expected.iter().flatten().any(|b| b.values.get(*name).is_some_and(|v| v.as_bigint_and_exponent() == (BigInt::from(0), scale))), "the fixture yields a zero {name} at scale {scale}");
	}
	(store, descriptors, expected)
}

/// Delete every frame, so a downsample can only be served from the sidecars.
fn remove_frames(descriptors: &[SegmentDescriptor]) {
	for descriptor in descriptors {
		std::fs::remove_file(&descriptor.path).expect("removes the frame");
	}
}

/// Sidecars sealed by this version: with every frame gone, the downsample is served from
/// them alone and still matches decode-then-reduce exponent for exponent.
#[tokio::test]
async fn sidecar_served_downsample_keeps_the_column_scale() {
	for scale in [3, 4, 6] {
		let dir = TempDir::new().expect("tempdir");
		let (store, descriptors, expected) = fixture(&dir, PhysicalType::ScaledI64 { scale }, i64::from(scale)).await;
		assert_matches(&store, &expected, &format!("scale {scale}, frames present")).await;
		remove_frames(&descriptors);
		assert_matches(&store, &expected, &format!("scale {scale}, sidecars only")).await;
	}
}

/// Sidecars written before the fix stored every zero as `"0"`, which reads back at
/// exponent 0. Loading one for a fixed-scale segment restores the segment's scale, so
/// those sidecars serve the same answer as new ones.
#[tokio::test]
async fn sidecars_with_unscaled_zeros_are_served_at_the_column_scale() {
	for scale in [3, 4, 6] {
		let dir = TempDir::new().expect("tempdir");
		let (store, descriptors, expected) = fixture(&dir, PhysicalType::ScaledI64 { scale }, i64::from(scale)).await;
		// Rewrite each sidecar as an older version read it back: every zero at exponent 0.
		for (seg, descriptor) in SEGMENTS.iter().zip(&descriptors) {
			let (ts, vs) = rows(seg, i64::from(scale));
			let points: Vec<Point> = ts.iter().zip(vs).map(|(t, v)| Point::new(DateTime::from_timestamp(*t, 0).expect("instant"), if v.is_zero() { BigDecimal::from(0) } else { v })).collect();
			let partial = reduce_partial(&points, BASE, None, None, &SIDECAR_AGGREGATIONS).expect("reduces");
			let sidecar = PartialSidecar::materialize(BASE, descriptor, partial, &TIERS).expect("materializes");
			std::fs::write(store.sidecar_path(ASPECT, descriptor.id).expect("path"), sidecar.to_bytes().expect("encodes")).expect("writes the sidecar");
		}
		remove_frames(&descriptors);
		assert_matches(&store, &expected, &format!("scale {scale}, unscaled sidecars")).await;
	}
}

/// A `Decimal128` column keeps each value's own scale, so no load-time rescale applies:
/// only the serializer carries a zero's scale, and a negative scale (`5E+2`), through the
/// sidecar.
#[tokio::test]
async fn decimal128_sidecars_keep_each_values_scale() {
	for scale in [3, -2] {
		let dir = TempDir::new().expect("tempdir");
		let (store, descriptors, expected) = fixture(&dir, PhysicalType::Decimal128, scale).await;
		remove_frames(&descriptors);
		assert_matches(&store, &expected, &format!("decimal128 at scale {scale}, sidecars only")).await;
	}
}
