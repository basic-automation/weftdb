// Portions adapted from DDSketch, sketches-java (https://github.com/DataDog/sketches-java),
// licensed under Apache-2.0; see THIRD-PARTY-NOTICES. Modifications: re-implemented in Rust
// with sparse ordered-map stores, BigDecimal entry points, typed errors and serde persistence.

//! # Mergeable approximate quantiles — `DDSketch`
//!
//! The [`Aggregation`](crate::Aggregation) percentiles are *exact* nearest-rank: they
//! materialize and sort the whole bucket, which costs `O(bucket)` memory and — the
//! sharper limit — **cannot be merged**. Two buckets' sorted sets say nothing about
//! their union's p99 without re-reading both, so an exact percentile can neither stream
//! nor combine across segments.
//!
//! [`DdSketch`] is the standard answer: a fixed-accuracy sketch with a **relative-error
//! guarantee** — for a quantile whose true value is `v`, the reported `v̂` satisfies
//! `|v̂ - v| ≤ α·|v|`. It is **fully mergeable** (merging two sketches is exact — it
//! yields the same sketch as feeding every sample into one), and its size grows with
//! the *log* of the value range rather than the sample count, so a bucket of any size
//! costs a bounded number of counters. This is what makes p99 latency monitoring
//! practical over large buckets and cross-segment reads.
//!
//! **This reduction is explicitly approximate**, which is why it is opt-in and named
//! apart from the exact percentiles: per WeftDB's precision principle the approximation is
//! *declared and bounded* (the `α` you choose), never a silent downcast. The exact
//! nearest-rank percentiles remain the default; reach for a sketch when the bucket is
//! too large to materialize or the result must merge.
//!
//! The mapping is the canonical logarithmic one: with `γ = (1+α)/(1-α)`, a positive `v`
//! lands in bucket `i = ⌈log_γ v⌉`, reported as `2γⁱ/(γ+1)` — the midpoint of the
//! bucket in the sense that guarantees the relative bound across the whole bucket.
//! Negative values use a mirrored store, and zero is counted exactly.
//!
//! *(src: `DDSketch`, PVLDB'19 — <https://dl.acm.org/doi/10.14778/3352063.3352135> ·
//! <https://arxiv.org/abs/1908.10693>)*
//!
//! Adapted from Datadog's Apache-2.0 reference implementation, sketches-java
//! (<https://github.com/DataDog/sketches-java>); the crate's THIRD-PARTY-NOTICES file carries
//! the attribution, its NOTICE text and the licence.

use std::collections::BTreeMap;

use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};

/// Every power of ten an `f64` holds exactly: `10^0` through `10^22`.
const EXACT_POWERS_OF_TEN: [f64; 23] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22];

/// `v` as the nearest `f64`, bit-identical to `bigdecimal`'s `to_f64`.
///
/// It is cheaper for the common case of at most 15–16 significant digits and at most 22
/// decimal places, which it converts by one exact division (both operands exact in `f64`, so
/// it rounds once, as `to_f64`'s correctly rounded parse does) instead
/// of formatting and parsing the decimal. Other values take `to_f64` itself. `None` when
/// `to_f64` gives none.
#[must_use]
pub fn decimal_to_f64(v: &BigDecimal) -> Option<f64> {
	let (digits, scale) = v.as_bigint_and_scale();
	digits.to_i64().and_then(|m| scaled_to_f64(m, scale)).or_else(|| v.to_f64())
}

/// `mantissa × 10^-scale` as the nearest `f64`, when `|mantissa| < 2^53` and
/// `0 <= scale <= 22`; `None` otherwise.
///
/// Under those bounds `mantissa as f64` and `10^scale` are both exact, so the one IEEE
/// division rounds the exact quotient once, to nearest: the result is the correctly
/// rounded image of the decimal. `bigdecimal`'s own `to_f64` returns that same value for
/// such a decimal (it formats the digits as `<digits>e-<scale>` and parses them with the
/// standard library's correctly rounded parser, trimming digits only past ~44), so this is
/// the same `f64` without the formatting and parsing. Correct rounding also makes it
/// independent of representation: `2.5` and `2.50` give the same `f64`.
fn scaled_to_f64(mantissa: i64, scale: i64) -> Option<f64> {
	const EXACT_MANTISSA: u64 = 1 << 53;
	if mantissa.unsigned_abs() >= EXACT_MANTISSA {
		return None;
	}
	let power = EXACT_POWERS_OF_TEN.get(usize::try_from(scale).ok()?)?;
	#[allow(clippy::cast_precision_loss)]
	let m = mantissa as f64;
	Some(m / power)
}

/// Why a sketch could not be built or fed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SketchError {
	/// The requested relative accuracy was not in the open interval `(0, 1)`.
	#[error("relative accuracy must be in (0, 1)")]
	InvalidAccuracy,
	/// A bucket budget of zero was requested; a sketch needs at least one bucket.
	#[error("max_bins must be at least 1")]
	InvalidMaxBins,
	/// Two sketches with different relative accuracies cannot be merged: the resulting
	/// bucket boundaries would not share a mapping, so no error bound would hold.
	#[error("cannot merge sketches with different relative accuracies")]
	AccuracyMismatch,
	/// A value could not be mapped into the sketch — it is not finite, or its magnitude
	/// falls outside the representable exponent range.
	#[error("a value is not representable in the sketch (non-finite or out of range)")]
	ValueRange,
}

/// A mergeable, fixed-relative-accuracy quantile sketch.
///
/// Feed values with [`add`](Self::add), combine with [`merge`](Self::merge), read with
/// [`quantile`](Self::quantile). Memory is bounded by the number of distinct occupied
/// buckets (logarithmic in the value range), not by the sample count.
///
/// Serde-serializable so a merged sketch can be persisted (e.g. inside a per-segment
/// [`PartialReduction`](crate::PartialReduction) sidecar) and reloaded exactly. The cached
/// `gamma`/`log_gamma` are stored rather than recomputed, so a round-trip reproduces the
/// mapping bit-for-bit and a deserialized sketch merges with a freshly built one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DdSketch {
	/// Relative accuracy `α` — the guaranteed relative error bound on every quantile.
	alpha: f64,
	/// `γ = (1+α)/(1-α)`; cached alongside its natural log (the hot divisor).
	gamma: f64,
	log_gamma: f64,
	/// Counts for strictly-positive values, keyed by bucket index (ascending).
	positive: BTreeMap<i32, u64>,
	/// Counts for strictly-negative values, keyed by the bucket index of `|v|`.
	negative: BTreeMap<i32, u64>,
	/// Exact count of zeros — zero has no logarithm, and is reported back exactly.
	zeros: u64,
	/// Total samples fed.
	count: u64,
	/// Maximum buckets per (positive/negative) store before the lowest are collapsed;
	/// `None` leaves the sketch unbounded. See [`DdSketch::with_max_bins`].
	max_bins: Option<usize>,
}

impl DdSketch {
	/// A sketch guaranteeing relative accuracy `alpha` on every quantile it reports.
	///
	/// A smaller `alpha` means tighter accuracy and more buckets; `0.01` (1%) is the
	/// common default for latency monitoring.
	///
	/// # Errors
	///
	/// [`SketchError::InvalidAccuracy`] unless `0 < alpha < 1`.
	pub fn new(alpha: f64) -> Result<Self, SketchError> {
		if !(alpha.is_finite() && alpha > 0.0 && alpha < 1.0) {
			return Err(SketchError::InvalidAccuracy);
		}
		Ok(Self::from_alpha(alpha))
	}

	/// A sketch at the crate's declared [`SKETCH_ALPHA`](crate::SKETCH_ALPHA) accuracy and
	/// [`SKETCH_MAX_BINS`](crate::SKETCH_MAX_BINS) bucket budget — what the `sketch_p*`
	/// reductions use, so their memory is bounded *absolutely* rather than by the value
	/// range.
	///
	/// Infallible: both constants are valid by construction, so the reduction path builds
	/// sketches without an unreachable error branch.
	#[must_use]
	pub fn with_default_accuracy() -> Self {
		let mut s = Self::from_alpha(crate::SKETCH_ALPHA);
		s.max_bins = Some(crate::SKETCH_MAX_BINS);
		s
	}

	/// A sketch that never exceeds `max_bins` buckets per sign store, collapsing the
	/// **lowest** buckets together when it would.
	///
	/// Without this, memory is bounded only by the *log of the value range* — small for a
	/// realistic column, but not absolutely bounded: a pathological range (sub-nanosecond
	/// next to astronomical) still allocates a bucket per magnitude step. `max_bins` makes
	/// the bound absolute, which is what the reference implementations do.
	///
	/// **The guarantee weakens where it collapses, and only there.** A `q`-quantile stays
	/// `α`-accurate as long as its value still falls in a surviving bucket; collapsed
	/// (lowest) buckets lose the relative-error bound for the quantiles inside them — for
	/// an all-positive column that is the *low* quantiles, and p95/p99 (the ones latency
	/// monitoring actually asks for) are unaffected. This is the documented behaviour of
	/// the reference `CollapsingLowestDenseStore`, not a WeftDB quirk; `UDDSketch` is the
	/// variant that keeps a uniform guarantee under collapse, and is filed as a follow-up.
	///
	/// Collapsing also costs **exact mergeability**: two sketches that each collapsed
	/// different buckets no longer merge to the same sketch a single pass would build.
	/// [`new`](Self::new) stays unbounded, so a caller that needs exact merges keeps it.
	///
	/// # Errors
	///
	/// [`SketchError::InvalidAccuracy`] unless `0 < alpha < 1`; [`SketchError::InvalidMaxBins`]
	/// if `max_bins` is zero.
	///
	/// *(src: bucket collapsing loses the guarantee on the collapsed quantiles —
	/// <https://github.com/DataDog/sketches-java> · `UDDSketch` —
	/// <https://arxiv.org/abs/2004.08604>)*
	pub fn with_max_bins(alpha: f64, max_bins: usize) -> Result<Self, SketchError> {
		if max_bins == 0 {
			return Err(SketchError::InvalidMaxBins);
		}
		let mut s = Self::new(alpha)?;
		s.max_bins = Some(max_bins);
		Ok(s)
	}

	/// Build the mapping for an already-validated `alpha`.
	fn from_alpha(alpha: f64) -> Self {
		let gamma = (1.0 + alpha) / (1.0 - alpha);
		Self { alpha, gamma, log_gamma: gamma.ln(), positive: BTreeMap::new(), negative: BTreeMap::new(), zeros: 0, count: 0, max_bins: None }
	}

	/// Collapse `store`'s lowest buckets together until it fits `max_bins`, folding each
	/// evicted bucket's count into the next-lowest survivor (so the total count — and
	/// therefore every rank — is preserved; only the *resolution* at the bottom is lost).
	fn collapse_lowest(store: &mut BTreeMap<i32, u64>, max_bins: usize) {
		while store.len() > max_bins {
			let Some((&lowest, _)) = store.iter().next() else { break };
			let Some(evicted) = store.remove(&lowest) else { break };
			// Fold into the new lowest; if the store just emptied, put it back.
			if let Some(next) = store.iter().next().map(|(&k, _)| k) {
				*store.entry(next).or_insert(0) += evicted;
			} else {
				store.insert(lowest, evicted);
				break;
			}
		}
	}

	/// Enforce `max_bins` on both sign stores.
	fn enforce_max_bins(&mut self) {
		if let Some(max) = self.max_bins {
			Self::collapse_lowest(&mut self.positive, max);
			Self::collapse_lowest(&mut self.negative, max);
		}
	}

	/// The relative accuracy `α` this sketch guarantees.
	#[must_use]
	pub const fn relative_accuracy(&self) -> f64 {
		self.alpha
	}

	/// Total number of samples fed.
	#[must_use]
	pub const fn count(&self) -> u64 {
		self.count
	}

	/// Whether no samples have been fed.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.count == 0
	}

	/// The number of occupied buckets — the sketch's real memory footprint, and the
	/// figure that stays bounded as the sample count grows.
	#[must_use]
	pub fn bucket_count(&self) -> usize {
		self.positive.len() + self.negative.len() + usize::from(self.zeros > 0)
	}

	/// The bucket index for a strictly-positive `v`: `⌈log_γ v⌉`.
	fn index(&self, v: f64) -> Result<i32, SketchError> {
		let raw = (v.ln() / self.log_gamma).ceil();
		if !raw.is_finite() || raw < f64::from(i32::MIN) || raw > f64::from(i32::MAX) {
			return Err(SketchError::ValueRange);
		}
		// `raw` is finite and inside i32's range, so the cast is exact.
		#[allow(clippy::cast_possible_truncation)]
		Ok(raw as i32)
	}

	/// The value reported for bucket `i`: `2γⁱ/(γ+1)`, the point whose relative distance
	/// to every value in the bucket is within `α`.
	fn value_of(&self, i: i32) -> f64 {
		2.0 * self.gamma.powi(i) / (self.gamma + 1.0)
	}

	/// Fold one `f64` sample into the sketch.
	///
	/// # Errors
	///
	/// [`SketchError::ValueRange`] if `v` is not finite or its magnitude falls outside
	/// the representable exponent range.
	pub fn add(&mut self, v: f64) -> Result<(), SketchError> {
		if !v.is_finite() {
			return Err(SketchError::ValueRange);
		}
		if v > 0.0 {
			let i = self.index(v)?;
			*self.positive.entry(i).or_insert(0) += 1;
		} else if v < 0.0 {
			let i = self.index(-v)?;
			*self.negative.entry(i).or_insert(0) += 1;
		} else {
			self.zeros += 1;
		}
		self.count += 1;
		self.enforce_max_bins();
		Ok(())
	}

	/// Fold one [`BigDecimal`] sample in, converting at this declared-approximate
	/// boundary. The sketch is a bounded-error structure by construction, so the `f64`
	/// conversion here is part of the declared approximation — not a silent downcast of
	/// an otherwise-exact reduction.
	///
	/// # Errors
	///
	/// [`SketchError::ValueRange`] if the value has no finite `f64` image.
	pub fn add_decimal(&mut self, v: &BigDecimal) -> Result<(), SketchError> {
		self.add(decimal_to_f64(v).ok_or(SketchError::ValueRange)?)
	}

	/// Fold in the sample `mantissa × 10^-scale` (a `ScaledI64` value), as
	/// [`add_decimal`](Self::add_decimal) would fold the equal `BigDecimal`, without
	/// building one when [`scaled_to_f64`] applies.
	pub(crate) fn add_scaled(&mut self, mantissa: i64, scale: i64) -> Result<(), SketchError> {
		match scaled_to_f64(mantissa, scale) {
			Some(f) => self.add(f),
			None => self.add_decimal(&BigDecimal::new(mantissa.into(), scale)),
		}
	}

	/// Merge `other` into `self`. Exact: the result is the sketch that feeding every
	/// sample of both into one sketch would have produced — the property the exact
	/// nearest-rank percentiles cannot offer.
	///
	/// # Errors
	///
	/// [`SketchError::AccuracyMismatch`] if the sketches were built with different
	/// relative accuracies (their buckets would not line up).
	pub fn merge(&mut self, other: &Self) -> Result<(), SketchError> {
		// Compare the mapping itself rather than the requested alpha: identical gamma is
		// exactly the condition under which the bucket boundaries coincide.
		if (self.gamma - other.gamma).abs() > f64::EPSILON * self.gamma.max(other.gamma) {
			return Err(SketchError::AccuracyMismatch);
		}
		for (&i, &c) in &other.positive {
			*self.positive.entry(i).or_insert(0) += c;
		}
		for (&i, &c) in &other.negative {
			*self.negative.entry(i).or_insert(0) += c;
		}
		self.zeros += other.zeros;
		self.count += other.count;
		self.enforce_max_bins();
		Ok(())
	}

	/// The approximate `q`-quantile (`q` in `[0, 1]`), within `α` relative error of the
	/// true value. `None` for an empty sketch or a `q` outside `[0, 1]`.
	///
	/// `q = 0` is the minimum, `q = 1` the maximum, `q = 0.5` the median.
	///
	/// **Rank convention — WeftDB's nearest-rank, deliberately not `DDSketch`'s reference
	/// rank.** The sample selected is the one at 1-based ordinal `⌈q·n⌉` (clamped to
	/// `1..=n`), which is exactly what the *exact* [`Aggregation`](crate::Aggregation)
	/// percentiles use. That makes `sketch_p*` and `p*` **substitutable at any bucket
	/// size**: they name the same sample, and the sketch's answer is then within `α` of
	/// the exact one. (The reference implementation ranks by `⌊q·(n-1)⌋` instead; on a
	/// small bucket that selects a *different* sample — `p99` of three samples would be
	/// the middle one rather than the largest — a divergence not worth inheriting.)
	///
	/// The `α` guarantee is relative to that sample's value; comparing against a
	/// differently-ranked sample can exceed `α` on densely-spaced data purely from the
	/// rank convention, with the mapping itself still exact.
	#[must_use]
	pub fn quantile(&self, q: f64) -> Option<f64> {
		if self.count == 0 || !(0.0..=1.0).contains(&q) {
			return None;
		}
		// Nearest-rank ordinal, matching the exact percentiles: ceil(q*n), 1-based, at
		// least 1. Integer arithmetic on the numerator keeps it exact for the ranks that
		// matter (q is a fixed 0.5/0.9/0.95/0.99 here). Values sort negative (descending
		// |v|) → zeros → positive, so accumulate in that order and take the bucket whose
		// cumulative count first reaches the ordinal.
		#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let ordinal = ((q * (self.count as f64)).ceil() as u64).clamp(1, self.count);
		let mut seen: u64 = 0;
		// Negatives, most-negative first: that is descending bucket index of |v|.
		for (&i, &c) in self.negative.iter().rev() {
			seen += c;
			if seen >= ordinal {
				return Some(-self.value_of(i));
			}
		}
		seen += self.zeros;
		if self.zeros > 0 && seen >= ordinal {
			return Some(0.0);
		}
		for (&i, &c) in &self.positive {
			seen += c;
			if seen >= ordinal {
				return Some(self.value_of(i));
			}
		}
		// Unreachable for a non-empty sketch (the ordinal is clamped to the count), but
		// fall back to the maximum rather than None.
		self.positive.keys().next_back().map(|&i| self.value_of(i)).or_else(|| if self.zeros > 0 { Some(0.0) } else { self.negative.keys().next().map(|&i| -self.value_of(i)) })
	}

	/// The approximate `q`-quantile as a [`BigDecimal`], for callers reducing in WeftDB's
	/// logical numeric type. `None` on the same conditions as [`quantile`](Self::quantile),
	/// or if the result has no `BigDecimal` image.
	#[must_use]
	pub fn quantile_decimal(&self, q: f64) -> Option<BigDecimal> {
		self.quantile(q).and_then(BigDecimal::from_f64)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn scaled_to_f64_is_bigdecimals_to_f64() {
		let mut state = 0x9e37_79b9_7f4a_7c15_u64;
		let mut next = move || {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state
		};
		let limit = (1_i64 << 53) - 1;
		let edges = [0, 1, -1, 7, -7, 5, 25, 1_234_567_891, limit, -limit, limit - 1, 4_503_599_627_370_497];
		for scale in 0..=22_i64 {
			let random = (0..4_000).map(|i| {
				// Mix full-range mantissas with short ones (prices), both signs.
				let m = (next() % (1 << 53)).cast_signed();
				let m = if i % 2 == 0 { m } else { m % 1_000_000 };
				if next() % 2 == 0 {
					m
				} else {
					-m
				}
			});
			for m in edges.into_iter().chain(random) {
				let expected = BigDecimal::new(m.into(), scale).to_f64().expect("finite");
				let got = scaled_to_f64(m, scale).expect("in range");
				assert_eq!(got.to_bits(), expected.to_bits(), "{m}e-{scale}: {got} vs {expected}");
			}
		}
		// Outside the bounds it declines, and the sketch falls back to `to_f64`.
		assert_eq!(scaled_to_f64(1 << 53, 2), None);
		assert_eq!(scaled_to_f64(i64::MIN, 2), None);
		assert_eq!(scaled_to_f64(5, 23), None);
		assert_eq!(scaled_to_f64(5, -1), None);
	}

	#[test]
	fn decimal_to_f64_is_to_f64_bit_for_bit() {
		for (m, s) in [(250_i64, 2_i64), (-1_999, 3), (0, 4), (123_456_789_012, 8), (9_007_199_254_740_993, 2), (7, 30), (-42, -2), (5, 0), (355_893, 2)] {
			let v = BigDecimal::new(m.into(), s);
			assert_eq!(decimal_to_f64(&v).map(f64::to_bits), v.to_f64().map(f64::to_bits), "{v}");
		}
		let huge: BigDecimal = "1e400".parse().expect("decimal");
		assert_eq!(decimal_to_f64(&huge).map(f64::to_bits), huge.to_f64().map(f64::to_bits));
	}

	#[test]
	fn add_decimal_and_add_scaled_build_the_same_sketch_as_to_f64() {
		let values: Vec<(i64, i64)> = vec![(250, 2), (25, 1), (-1_999, 3), (0, 4), (123_456_789_012, 8), (9_007_199_254_740_993, 2), (7, 30), (-42, -2), (5, 0)];
		let (mut reference, mut decimal, mut scaled) = (DdSketch::with_default_accuracy(), DdSketch::with_default_accuracy(), DdSketch::with_default_accuracy());
		for &(m, s) in &values {
			let v = BigDecimal::new(m.into(), s);
			reference.add(v.to_f64().expect("finite")).expect("adds");
			decimal.add_decimal(&v).expect("adds");
			scaled.add_scaled(m, s).expect("adds");
		}
		assert_eq!(decimal, reference);
		assert_eq!(scaled, reference);
	}

	/// The guarantee that justifies the whole structure: every reported quantile is
	/// within α relative error of the true value, checked against an exact sort.
	#[test]
	fn every_quantile_is_within_the_relative_error_bound() {
		let alpha = 0.01;
		let mut s = DdSketch::new(alpha).expect("valid accuracy");
		// A wide, skewed range — the regime sketches exist for (latency-like).
		let values: Vec<f64> = (1..=10_000).map(|i| f64::from(i) * 1.7).collect();
		for &v in &values {
			s.add(v).expect("in range");
		}
		let mut sorted = values;
		sorted.sort_by(f64::total_cmp);
		for &q in &[0.0, 0.01, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99, 1.0] {
			// The sketch's rank convention is WeftDB's nearest-rank — 1-based ordinal
			// ceil(q*n), clamped — the same sample the exact percentiles name.
			#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
			let ordinal = ((q * (sorted.len() as f64)).ceil() as usize).clamp(1, sorted.len());
			let exact = sorted[ordinal - 1];
			let got = s.quantile(q).expect("non-empty");
			let rel = (got - exact).abs() / exact.abs();
			assert!(rel <= alpha, "q={q}: got {got}, exact {exact}, relative error {rel} exceeds alpha {alpha}");
		}
	}

	/// Mergeability — the property the exact percentiles cannot provide. Merging two
	/// sketches must equal feeding every sample into one.
	#[test]
	fn merge_equals_feeding_every_sample_into_one_sketch() {
		let mut a = DdSketch::new(0.01).unwrap();
		let mut b = DdSketch::new(0.01).unwrap();
		let mut whole = DdSketch::new(0.01).unwrap();
		for i in 1..=500 {
			let v = f64::from(i);
			a.add(v).unwrap();
			whole.add(v).unwrap();
		}
		for i in 501..=1000 {
			let v = f64::from(i);
			b.add(v).unwrap();
			whole.add(v).unwrap();
		}
		a.merge(&b).expect("same accuracy merges");
		assert_eq!(a.count(), whole.count(), "merged count matches");
		assert_eq!(a, whole, "a merged sketch is bit-for-bit the single-pass sketch");
		for &q in &[0.0, 0.5, 0.9, 0.99, 1.0] {
			assert!((a.quantile(q).unwrap() - whole.quantile(q).unwrap()).abs() < f64::EPSILON, "q={q} agrees after merge");
		}
	}

	#[test]
	fn merge_is_order_independent_and_rejects_mismatched_accuracy() {
		let (mut x, mut y) = (DdSketch::new(0.02).unwrap(), DdSketch::new(0.02).unwrap());
		for i in 1..=100 {
			x.add(f64::from(i)).unwrap();
			y.add(f64::from(i) * 3.0).unwrap();
		}
		let (mut xy, mut yx) = (x.clone(), y.clone());
		xy.merge(&y).unwrap();
		yx.merge(&x).unwrap();
		assert_eq!(xy, yx, "merge is commutative");

		let other = DdSketch::new(0.05).unwrap();
		assert_eq!(x.merge(&other), Err(SketchError::AccuracyMismatch), "differing accuracies cannot merge");
	}

	/// Negative values, zero, and a range straddling all three.
	#[test]
	fn handles_negatives_zero_and_a_straddling_range() {
		let mut s = DdSketch::new(0.01).unwrap();
		let values = [-100.0, -10.0, -1.0, 0.0, 1.0, 10.0, 100.0];
		for v in values {
			s.add(v).unwrap();
		}
		assert_eq!(s.count(), 7);
		assert!((s.quantile(0.0).unwrap() - (-100.0)).abs() / 100.0 <= 0.01, "min is the most-negative value");
		assert!((s.quantile(1.0).unwrap() - 100.0).abs() / 100.0 <= 0.01, "max is the largest value");
		assert!(s.quantile(0.5).unwrap().abs() < f64::EPSILON, "median of this symmetric set is exactly 0");
	}

	#[test]
	fn zero_is_reported_exactly() {
		let mut s = DdSketch::new(0.01).unwrap();
		for _ in 0..10 {
			s.add(0.0).unwrap();
		}
		assert_eq!(s.quantile(0.5), Some(0.0), "a zeros-only sketch reports exactly 0, not an approximation");
		assert_eq!(s.bucket_count(), 1, "zeros occupy one counter regardless of count");
	}

	#[test]
	fn empty_and_out_of_range_quantiles_are_none() {
		let mut s = DdSketch::new(0.01).unwrap();
		assert!(s.is_empty());
		assert_eq!(s.quantile(0.5), None, "an empty sketch has no quantile");
		s.add(1.0).unwrap();
		assert_eq!(s.quantile(-0.1), None, "q below 0 is rejected");
		assert_eq!(s.quantile(1.1), None, "q above 1 is rejected");
	}

	#[test]
	fn single_value_and_repeated_value_are_within_bound() {
		let mut s = DdSketch::new(0.01).unwrap();
		s.add(42.0).unwrap();
		let got = s.quantile(0.5).unwrap();
		assert!((got - 42.0).abs() / 42.0 <= 0.01, "a single sample is its own quantile within alpha");
		for _ in 0..999 {
			s.add(42.0).unwrap();
		}
		assert_eq!(s.bucket_count(), 1, "1000 identical samples occupy exactly one bucket");
		assert!((s.quantile(0.99).unwrap() - 42.0).abs() / 42.0 <= 0.01);
	}

	/// The memory claim: bucket count grows with the log of the *range*, not with n.
	#[test]
	fn memory_is_bounded_by_the_value_range_not_the_sample_count() {
		let mut s = DdSketch::new(0.01).unwrap();
		for i in 1..=100_000_u32 {
			// 100k samples, all within [1, 10) — a narrow range.
			s.add(1.0 + f64::from(i % 9)).unwrap();
		}
		assert_eq!(s.count(), 100_000);
		assert!(s.bucket_count() <= 120, "100k samples over a narrow range stay in a handful of buckets, got {}", s.bucket_count());
	}

	/// The absolute memory bound: an extreme dynamic range must not outgrow the budget.
	#[test]
	fn max_bins_bounds_memory_over_a_pathological_range() {
		let mut s = DdSketch::with_max_bins(0.01, 16).expect("valid");
		// 1e-9 .. 1e9 — a range that unbounded would occupy thousands of buckets.
		for e in -9_i32..=9 {
			for m in 1..=9 {
				s.add(f64::from(m) * 10_f64.powi(e)).expect("in range");
			}
		}
		assert!(s.bucket_count() <= 16, "the store must never exceed max_bins, got {}", s.bucket_count());
		assert_eq!(s.count(), 19 * 9, "collapsing preserves every sample's count");
		// The top of the distribution keeps its guarantee — collapsing only ate the bottom.
		let max = s.quantile(1.0).expect("non-empty");
		assert!((max - 9e9).abs() / 9e9 <= 0.01, "the maximum stays within alpha after collapsing, got {max}");
	}

	/// Collapsing is not free, and the test says exactly what it costs: ranks survive, the
	/// low quantiles' relative-error bound does not.
	#[test]
	fn collapsing_preserves_ranks_but_not_the_low_quantile_bound() {
		let (mut bounded, mut exact) = (DdSketch::with_max_bins(0.01, 4).expect("valid"), DdSketch::new(0.01).expect("valid"));
		for e in 0_i32..8 {
			bounded.add(10_f64.powi(e)).expect("in range");
			exact.add(10_f64.powi(e)).expect("in range");
		}
		assert!(bounded.bucket_count() <= 4);
		assert_eq!(bounded.count(), exact.count(), "no sample is lost");
		// The highest value is in a surviving bucket, so it keeps the bound.
		assert!((bounded.quantile(1.0).unwrap() - exact.quantile(1.0).unwrap()).abs() < f64::EPSILON, "the max is unaffected by collapsing the bottom");
		// The lowest is inside a collapsed bucket, so it does NOT — asserted, not hidden.
		assert!(bounded.quantile(0.0).unwrap() > exact.quantile(0.0).unwrap(), "a collapsed low quantile reads high — the documented cost of max_bins");
	}

	#[test]
	fn default_accuracy_sketch_is_bounded_and_rejects_a_zero_budget() {
		assert_eq!(DdSketch::with_max_bins(0.01, 0).unwrap_err(), SketchError::InvalidMaxBins);
		// The reduction's sketch carries the budget, so `sketch_p*` memory is absolutely bounded.
		let mut s = DdSketch::with_default_accuracy();
		for i in 1..=10_000 {
			s.add(f64::from(i)).expect("in range");
		}
		assert!(s.bucket_count() <= crate::SKETCH_MAX_BINS, "the default sketch respects the budget");
		// ...and the budget is loose enough that a realistic column never collapses.
		let exact = (1..=10_000).map(f64::from).collect::<Vec<_>>();
		let got = s.quantile(0.99).expect("non-empty");
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let want = exact[(0.99_f64 * 9_999.0).floor() as usize];
		assert!((got - want).abs() / want <= 0.01, "no collapse on a realistic range, so p99 keeps alpha");
	}

	#[test]
	fn rejects_invalid_accuracy_and_non_finite_values() {
		assert_eq!(DdSketch::new(0.0).unwrap_err(), SketchError::InvalidAccuracy);
		assert_eq!(DdSketch::new(1.0).unwrap_err(), SketchError::InvalidAccuracy);
		assert_eq!(DdSketch::new(f64::NAN).unwrap_err(), SketchError::InvalidAccuracy);
		let mut s = DdSketch::new(0.01).unwrap();
		assert_eq!(s.add(f64::NAN), Err(SketchError::ValueRange));
		assert_eq!(s.add(f64::INFINITY), Err(SketchError::ValueRange));
		assert_eq!(s.count(), 0, "a rejected value does not count");
	}

	#[test]
	fn bigdecimal_boundary_round_trips() {
		use std::str::FromStr;
		let mut s = DdSketch::new(0.01).unwrap();
		for v in ["1.5", "2.25", "100.125"] {
			s.add_decimal(&BigDecimal::from_str(v).unwrap()).unwrap();
		}
		let median = s.quantile_decimal(0.5).expect("median");
		let exact = BigDecimal::from_str("2.25").unwrap();
		let rel = ((&median - &exact) / &exact).to_f64().unwrap().abs();
		assert!(rel <= 0.01, "the BigDecimal boundary keeps the alpha bound, got relative error {rel}");
	}
}
