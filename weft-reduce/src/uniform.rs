//! # Mergeable quantiles with a uniform guarantee under collapse — `UDDSketch`
//!
//! [`DdSketch`](crate::DdSketch) bounds its memory by folding its **lowest** buckets
//! together. That keeps the high quantiles exact to `α` and gives up the bound entirely on
//! the quantiles that fall in the folded buckets, and two sketches that folded different
//! buckets no longer merge to the sketch a single pass would have built.
//!
//! [`UddSketch`] bounds memory the other way. When it runs out of buckets it merges every
//! **adjacent pair** of buckets, over the whole value range at once: bucket indices `2j-1`
//! and `2j` become index `j`, which is exactly the bucket layout of a sketch built with
//! `γ² ` in place of `γ`. Every quantile therefore keeps a relative-error bound, the same
//! one for all of them, and that bound is known: after `k` collapses it is
//! `α_k = tanh(2^k · atanh α)` (one collapse: `2α / (1 + α²)`).
//! [`relative_accuracy`](UddSketch::relative_accuracy) reports it, so the approximation
//! stays declared and bounded as it degrades.
//!
//! Because the collapse depends only on which buckets are occupied, the state of a sketch
//! is a function of the *set of samples* it has seen and its budget — not of their order,
//! and not of how they were split across sketches. So merging stays **exact** under
//! collapse: two sketches with the same accuracy and budget merge to the very sketch one
//! pass over all the samples would have built.
//!
//! The price is that the bound loosens everywhere, including on the high quantiles that
//! a lowest-bucket collapse would have left alone. Which trade is better depends on which
//! quantiles are read; this type exists so the choice can be made.
//!
//! *(src: `UDDSketch`, Epicoco, Melle, Cafaro, Pulimeno, Morleo —
//! <https://arxiv.org/abs/2004.08604>)*

use std::collections::BTreeMap;

use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};

use crate::sketch::{decimal_to_f64, SketchError};

/// The smallest bucket budget a [`UddSketch`] accepts.
///
/// Collapsing halves bucket indices toward `0` and `1`, where it stops: the bucket of
/// values at or below one and the bucket of values above it never merge. With both signs
/// present that leaves four buckets that no amount of collapsing can reduce.
pub const UDD_MIN_BINS: usize = 4;

/// A mergeable quantile sketch whose relative-error bound is the same for every quantile,
/// before and after it collapses to fit its bucket budget.
///
/// Feed values with [`add`](Self::add), combine with [`merge`](Self::merge), read with
/// [`quantile`](Self::quantile), and read the bound those quantiles currently carry with
/// [`relative_accuracy`](Self::relative_accuracy). See the [module docs](self) for how it
/// differs from [`DdSketch`](crate::DdSketch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UddSketch {
	/// The relative accuracy the sketch was built with, before any collapse.
	initial_alpha: f64,
	/// `ln γ` of the current mapping: the initial `ln((1+α)/(1-α))`, doubled per collapse.
	log_gamma: f64,
	/// How many times every adjacent pair of buckets has been merged.
	collapses: u32,
	/// Counts for strictly-positive values, keyed by bucket index (ascending).
	positive: BTreeMap<i32, u64>,
	/// Counts for strictly-negative values, keyed by the bucket index of `|v|`.
	negative: BTreeMap<i32, u64>,
	/// Exact count of zeros.
	zeros: u64,
	/// Total samples fed.
	count: u64,
	/// Maximum occupied buckets over both sign stores together (the zero counter is extra).
	max_bins: usize,
}

/// `⌈i / 2⌉`: the index bucket `i` takes when adjacent pairs merge.
const fn half_ceil(i: i32) -> i32 {
	i.div_euclid(2) + i.rem_euclid(2)
}

impl UddSketch {
	/// A sketch that starts at relative accuracy `alpha` and never holds more than
	/// `max_bins` buckets over its positive and negative stores together.
	///
	/// # Errors
	///
	/// [`SketchError::InvalidAccuracy`] unless `0 < alpha < 1`;
	/// [`SketchError::UniformBudgetTooSmall`] if `max_bins` is below [`UDD_MIN_BINS`].
	pub fn new(alpha: f64, max_bins: usize) -> Result<Self, SketchError> {
		if !(alpha.is_finite() && alpha > 0.0 && alpha < 1.0) {
			return Err(SketchError::InvalidAccuracy);
		}
		if max_bins < UDD_MIN_BINS {
			return Err(SketchError::UniformBudgetTooSmall);
		}
		let log_gamma = ((1.0 + alpha) / (1.0 - alpha)).ln();
		Ok(Self { initial_alpha: alpha, log_gamma, collapses: 0, positive: BTreeMap::new(), negative: BTreeMap::new(), zeros: 0, count: 0, max_bins })
	}

	/// The relative-error bound every quantile carries **now**: the accuracy the sketch was
	/// built with until it first collapses, and `tanh(2^k · atanh α)` after `k` collapses.
	#[must_use]
	pub fn relative_accuracy(&self) -> f64 {
		if self.collapses == 0 {
			self.initial_alpha
		} else {
			(self.log_gamma / 2.0).tanh()
		}
	}

	/// The relative accuracy the sketch was built with.
	#[must_use]
	pub const fn initial_relative_accuracy(&self) -> f64 {
		self.initial_alpha
	}

	/// How many times the sketch has collapsed. Zero means every quantile still carries
	/// the accuracy it was built with.
	#[must_use]
	pub const fn collapses(&self) -> u32 {
		self.collapses
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

	/// The number of occupied buckets, counting the zero counter when it is in use.
	#[must_use]
	pub fn bucket_count(&self) -> usize {
		self.positive.len() + self.negative.len() + usize::from(self.zeros > 0)
	}

	/// The bucket index for a strictly-positive `v` under the current mapping.
	fn index(&self, v: f64) -> Result<i32, SketchError> {
		(v.ln() / self.log_gamma).ceil().to_i32().ok_or(SketchError::ValueRange)
	}

	/// The value reported for bucket `i`: `2γⁱ/(γ+1)`, within the current accuracy of every
	/// value in the bucket.
	///
	/// Evaluated as `exp((i-1)·ln γ + ln 2 − ln(1 + γ⁻¹))`, which stays finite when `γ`
	/// itself has grown past `f64` after many collapses.
	fn value_of(&self, i: i32) -> f64 {
		let exponent = (f64::from(i) - 1.0).mul_add(self.log_gamma, std::f64::consts::LN_2) - (-self.log_gamma).exp().ln_1p();
		exponent.exp().min(f64::MAX)
	}

	/// Merge every adjacent pair of buckets in both stores, squaring `γ`.
	fn collapse(&mut self) {
		for store in [&mut self.positive, &mut self.negative] {
			let mut halved = BTreeMap::new();
			for (&i, &c) in store.iter() {
				*halved.entry(half_ceil(i)).or_insert(0) += c;
			}
			*store = halved;
		}
		self.log_gamma *= 2.0;
		self.collapses += 1;
	}

	/// Collapse until both stores together fit the budget.
	///
	/// It ends: thirty-one halvings take every `i32` index to `0` or `1`, which is two
	/// buckets a store and within any budget [`new`](Self::new) accepts.
	fn enforce_max_bins(&mut self) {
		while self.positive.len() + self.negative.len() > self.max_bins && (self.log_gamma * 2.0).is_finite() {
			self.collapse();
		}
	}

	/// Fold one `f64` sample into the sketch.
	///
	/// # Errors
	///
	/// [`SketchError::ValueRange`] if `v` is not finite or its magnitude falls outside the
	/// representable exponent range. A rejected value changes nothing.
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

	/// Fold one [`BigDecimal`] sample in, converting to `f64` at this declared-approximate
	/// boundary, as [`DdSketch::add_decimal`](crate::DdSketch::add_decimal) does.
	///
	/// # Errors
	///
	/// [`SketchError::ValueRange`] if the value has no finite `f64` image.
	pub fn add_decimal(&mut self, v: &BigDecimal) -> Result<(), SketchError> {
		self.add(decimal_to_f64(v).ok_or(SketchError::ValueRange)?)
	}

	/// Merge `other` into `self`.
	///
	/// The less-collapsed side is first collapsed to the other's level, so the buckets line
	/// up; then the counts add, and the result collapses further if it is over `self`'s
	/// budget. When both sketches have the same budget the result is exactly the sketch that
	/// feeding every sample of both into one would have built, collapsed or not.
	///
	/// # Errors
	///
	/// [`SketchError::AccuracyMismatch`] if the sketches were built with different relative
	/// accuracies: their buckets coincide at no collapse level.
	pub fn merge(&mut self, other: &Self) -> Result<(), SketchError> {
		if self.initial_alpha.to_bits() != other.initial_alpha.to_bits() {
			return Err(SketchError::AccuracyMismatch);
		}
		while self.collapses < other.collapses {
			self.collapse();
		}
		let behind = self.collapses - other.collapses;
		let lift = |i: i32| (0..behind).fold(i, |i, _| half_ceil(i));
		for (&i, &c) in &other.positive {
			*self.positive.entry(lift(i)).or_insert(0) += c;
		}
		for (&i, &c) in &other.negative {
			*self.negative.entry(lift(i)).or_insert(0) += c;
		}
		self.zeros += other.zeros;
		self.count += other.count;
		self.enforce_max_bins();
		Ok(())
	}

	/// The approximate `q`-quantile (`q` in `[0, 1]`), within
	/// [`relative_accuracy`](Self::relative_accuracy) of the true value. `None` for an empty
	/// sketch or a `q` outside `[0, 1]`.
	///
	/// The rank convention is the crate's nearest-rank one — the sample at 1-based ordinal
	/// `⌈q·n⌉` — the same as [`DdSketch::quantile`](crate::DdSketch::quantile) and the exact
	/// percentiles.
	#[must_use]
	pub fn quantile(&self, q: f64) -> Option<f64> {
		if self.count == 0 || !(0.0..=1.0).contains(&q) {
			return None;
		}
		let ordinal = (q * self.count.to_f64().unwrap_or(f64::MAX)).ceil().to_u64().unwrap_or(0).clamp(1, self.count);
		let mut seen: u64 = 0;
		// Values sort negative (descending |v|), then zeros, then positive.
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
		// Not reached for a non-empty sketch: the ordinal is clamped to the count.
		None
	}

	/// The approximate `q`-quantile as a [`BigDecimal`]. `None` on the same conditions as
	/// [`quantile`](Self::quantile), or if the result has no `BigDecimal` image.
	#[must_use]
	pub fn quantile_decimal(&self, q: f64) -> Option<BigDecimal> {
		self.quantile(q).and_then(BigDecimal::from_f64)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::DdSketch;

	/// Slack for the floating-point evaluation of a bound that holds exactly in the reals.
	const FUZZ: f64 = 1e-12;

	const QUANTILES: [f64; 11] = [0.0, 0.001, 0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99, 0.999, 1.0];

	/// The exact nearest-rank sample of `sorted` at `q`.
	fn exact(sorted: &[f64], q: f64) -> f64 {
		let n = sorted.len();
		let ordinal = (q * n.to_f64().expect("small")).ceil().to_usize().unwrap_or(0).clamp(1, n);
		sorted[ordinal - 1]
	}

	/// The worst relative error of `quantile` over [`QUANTILES`], against an exact sort.
	fn worst_error(values: &[f64], quantile: impl Fn(f64) -> Option<f64>) -> f64 {
		let mut sorted = values.to_vec();
		sorted.sort_by(f64::total_cmp);
		QUANTILES
			.iter()
			.map(|&q| {
				let want = exact(&sorted, q);
				let got = quantile(q).expect("non-empty");
				if want == 0.0 {
					got.abs()
				} else {
					(got - want).abs() / want.abs()
				}
			})
			.fold(0.0, f64::max)
	}

	/// Eighteen decades, 1e-9 to 9e9: thousands of buckets at 1% if nothing collapsed.
	fn wide_range() -> Vec<f64> {
		(-9_i32..=9).flat_map(|e| (1..=9).map(move |m| f64::from(m) * 10_f64.powi(e))).collect()
	}

	fn xorshift(seed: u64) -> impl FnMut() -> u64 {
		let mut state = seed;
		move || {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state
		}
	}

	#[test]
	fn half_ceil_is_the_ceiling_of_half() {
		for (i, want) in [(0, 0), (1, 1), (2, 1), (3, 2), (4, 2), (-1, 0), (-2, -1), (-3, -1), (-4, -2), (i32::MAX, 1 << 30), (i32::MIN, -(1 << 30))] {
			assert_eq!(half_ceil(i), want, "{i}");
		}
	}

	#[test]
	fn without_a_collapse_it_keeps_the_accuracy_it_was_built_with() {
		let mut s = UddSketch::new(0.01, 2048).expect("valid");
		let values: Vec<f64> = (1..=10_000).map(|i| f64::from(i) * 1.7).collect();
		for &v in &values {
			s.add(v).expect("in range");
		}
		assert_eq!(s.collapses(), 0);
		assert_eq!(s.relative_accuracy().to_bits(), 0.01_f64.to_bits());
		let worst = worst_error(&values, |q| s.quantile(q));
		assert!(worst <= 0.01 + FUZZ, "worst relative error {worst}");
	}

	/// The reason the type exists: after collapsing, every quantile — the lowest included —
	/// is within the accuracy the sketch reports, where a lowest-bucket collapse on the same
	/// data and budget loses the low quantiles.
	#[test]
	fn every_quantile_keeps_the_reported_bound_after_collapsing() {
		let values = wide_range();
		let mut uniform = UddSketch::new(0.01, 16).expect("valid");
		let mut lowest = DdSketch::with_max_bins(0.01, 16).expect("valid");
		for &v in &values {
			uniform.add(v).expect("in range");
			lowest.add(v).expect("in range");
		}
		assert!(uniform.collapses() > 0, "this range does not fit sixteen buckets at 1%");
		assert!(uniform.bucket_count() <= 16, "got {}", uniform.bucket_count());
		assert_eq!(uniform.count(), 19 * 9, "collapsing keeps every sample's count");

		let bound = uniform.relative_accuracy();
		assert!(bound > 0.01 && bound < 1.0, "the bound loosened and is still a bound: {bound}");
		let worst = worst_error(&values, |q| uniform.quantile(q));
		assert!(worst <= bound + FUZZ, "worst relative error {worst} exceeds the reported {bound}");

		// The lowest-bucket collapse reads its minimum from the folded bucket, far outside
		// any relative bound.
		let worst_lowest = worst_error(&values, |q| lowest.quantile(q));
		assert!(worst_lowest > 1.0, "expected the folded low quantiles to be off by more than 100%, got {worst_lowest}");
	}

	#[test]
	fn the_reported_bound_is_the_closed_form() {
		let alpha = 0.01_f64;
		let mut s = UddSketch::new(alpha, UDD_MIN_BINS).expect("valid");
		s.collapse();
		let once = 2.0 * alpha / alpha.mul_add(alpha, 1.0);
		assert!((s.relative_accuracy() - once).abs() < FUZZ, "one collapse is 2α/(1+α²)");
		for k in 2..=6_i32 {
			s.collapse();
			let want = (2.0_f64.powi(k) * alpha.atanh()).tanh();
			assert!((s.relative_accuracy() - want).abs() < FUZZ, "k = {k}");
		}
		assert_eq!(s.collapses(), 6);
		assert_eq!(s.initial_relative_accuracy().to_bits(), alpha.to_bits());
	}

	/// A sample indexed after `k` collapses lands in the bucket its uncollapsed index
	/// collapses to. This is what makes the sketch a function of its samples.
	#[test]
	fn indexing_after_a_collapse_agrees_with_collapsing_the_index() {
		let fine = UddSketch::new(0.01, UDD_MIN_BINS).expect("valid");
		let mut next = xorshift(0x243f_6a88_85a3_08d3);
		let values: Vec<f64> = (0..20_000).map(|_| f64::from_bits(next() % 0x7fef_ffff_ffff_ffff + 1)).filter(|v| v.is_finite() && *v > 0.0).collect();
		let mut coarse = fine.clone();
		for k in 1..=12_u32 {
			coarse.collapse();
			for &v in &values {
				let Ok(i) = fine.index(v) else { continue };
				let lifted = (0..k).fold(i, |i, _| half_ceil(i));
				assert_eq!(coarse.index(v), Ok(lifted), "{v:e} after {k} collapses");
			}
		}
	}

	/// Merging stays exact under collapse, however the samples were split and ordered.
	#[test]
	fn merging_collapsed_sketches_equals_one_pass_over_every_sample() {
		let mut next = xorshift(0x9e37_79b9_7f4a_7c15);
		let mut values = wide_range();
		values.extend(wide_range().iter().map(|v| -v * 3.0));
		values.extend([0.0, 0.0]);
		for i in (1..values.len()).rev() {
			let j = usize::try_from(next() % u64::try_from(i + 1).expect("small")).expect("small");
			values.swap(i, j);
		}
		let mut whole = UddSketch::new(0.01, 24).expect("valid");
		for &v in &values {
			whole.add(v).expect("in range");
		}
		assert!(whole.collapses() > 0);

		for split in [1, 7, values.len() / 2, values.len() - 3] {
			let (mut a, mut b) = (UddSketch::new(0.01, 24).expect("valid"), UddSketch::new(0.01, 24).expect("valid"));
			for &v in &values[..split] {
				a.add(v).expect("in range");
			}
			for &v in &values[split..] {
				b.add(v).expect("in range");
			}
			let (mut ab, mut ba) = (a.clone(), b.clone());
			ab.merge(&b).expect("same accuracy");
			ba.merge(&a).expect("same accuracy");
			assert_eq!(ab, whole, "split at {split}: merged is the one-pass sketch");
			assert_eq!(ba, whole, "split at {split}: in either order");
		}
	}

	#[test]
	fn negatives_zero_and_positives_share_one_bound() {
		let mut values = wide_range();
		values.extend(wide_range().iter().map(|v| -v));
		values.extend([0.0; 5]);
		let mut s = UddSketch::new(0.02, 32).expect("valid");
		for &v in &values {
			s.add(v).expect("in range");
		}
		assert!(s.bucket_count() <= 33, "thirty-two buckets and the zero counter, got {}", s.bucket_count());
		assert_eq!(s.quantile(0.5), Some(0.0), "the median of this symmetric set is exactly zero");
		let worst = worst_error(&values, |q| s.quantile(q));
		assert!(worst <= s.relative_accuracy() + FUZZ, "worst {worst} against {}", s.relative_accuracy());
	}

	/// The whole `f64` range in the smallest budget: it terminates, stays in budget, and
	/// still reports finite values of the right sign and order of bucket.
	#[test]
	fn the_full_f64_range_fits_the_smallest_budget() {
		let mut s = UddSketch::new(0.01, UDD_MIN_BINS).expect("valid");
		let values = [f64::from_bits(1), 1e-300, 0.5, 1.0, 2.0, 1e300, f64::MAX, -f64::from_bits(1), -1.0, -f64::MAX];
		for v in values {
			s.add(v).expect("finite");
		}
		assert!(s.bucket_count() <= UDD_MIN_BINS, "got {}", s.bucket_count());
		assert_eq!(s.count(), 10);
		let bound = s.relative_accuracy();
		assert!(bound <= 1.0, "a relative bound never exceeds 1, got {bound}");
		for q in QUANTILES {
			let got = s.quantile(q).expect("non-empty");
			assert!(got.is_finite(), "q = {q} gave {got}");
		}
		assert!(s.quantile(0.0).expect("non-empty") < 0.0);
		assert!(s.quantile(1.0).expect("non-empty") > 0.0);
	}

	#[test]
	fn it_survives_the_sidecar_codec() {
		let mut s = UddSketch::new(0.01, 16).expect("valid");
		for v in wide_range() {
			s.add(v).expect("in range");
		}
		let bytes = postcard::to_allocvec(&s).expect("serializes");
		let back: UddSketch = postcard::from_bytes(&bytes).expect("deserializes");
		assert_eq!(back, s);
		let mut merged = back;
		merged.merge(&s).expect("a reloaded sketch merges with the original");
		assert_eq!(merged.count(), 2 * s.count());
	}

	#[test]
	fn the_decimal_boundary_matches_the_f64_one() {
		use std::str::FromStr;
		let (mut decimal, mut float) = (UddSketch::new(0.01, 64).expect("valid"), UddSketch::new(0.01, 64).expect("valid"));
		for v in ["1.5", "2.25", "100.125", "-3.75", "0"] {
			let d = BigDecimal::from_str(v).expect("decimal");
			decimal.add_decimal(&d).expect("adds");
			float.add(d.to_f64().expect("finite")).expect("adds");
		}
		assert_eq!(decimal, float);
		let median = decimal.quantile_decimal(0.5).expect("median");
		let rel = ((&median - BigDecimal::from_str("1.5").expect("decimal")) / BigDecimal::from_str("1.5").expect("decimal")).to_f64().expect("finite").abs();
		assert!(rel <= 0.01 + FUZZ, "got relative error {rel}");
	}

	#[test]
	fn it_rejects_what_it_cannot_hold() {
		assert_eq!(UddSketch::new(0.0, 16).unwrap_err(), SketchError::InvalidAccuracy);
		assert_eq!(UddSketch::new(1.0, 16).unwrap_err(), SketchError::InvalidAccuracy);
		assert_eq!(UddSketch::new(f64::NAN, 16).unwrap_err(), SketchError::InvalidAccuracy);
		assert_eq!(UddSketch::new(0.01, UDD_MIN_BINS - 1).unwrap_err(), SketchError::UniformBudgetTooSmall);

		let mut s = UddSketch::new(0.01, 16).expect("valid");
		assert_eq!(s.quantile(0.5), None, "an empty sketch has no quantile");
		assert_eq!(s.add(f64::NAN), Err(SketchError::ValueRange));
		assert_eq!(s.add(f64::NEG_INFINITY), Err(SketchError::ValueRange));
		assert!(s.is_empty(), "a rejected value does not count");
		s.add(1.0).expect("in range");
		assert_eq!(s.quantile(-0.1), None);
		assert_eq!(s.quantile(1.1), None);

		let other = UddSketch::new(0.02, 16).expect("valid");
		assert_eq!(s.merge(&other), Err(SketchError::AccuracyMismatch));
	}
}
