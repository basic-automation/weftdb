use std::{fmt::Display, str::FromStr};

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use splimes::Spline;
use uuid::Uuid;
use wide::f64x4;

use crate::types::{
	database::helpers::{safe_ratio, safe_usize_to_f64}, measurement_vector::MeasurementVector, pattern::Pattern, relative::Relative
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DictionaryId(Uuid);

impl DictionaryId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	#[must_use]
	pub const fn as_uuid(&self) -> Uuid {
		self.0
	}
}

impl Default for DictionaryId {
	fn default() -> Self {
		Self::new()
	}
}

impl Display for DictionaryId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dictionary {
	id: DictionaryId,
	name: String,
	description: String,
	patterns: Vec<Pattern>,
	constraints: DictionaryConstraints,
	/// Thresholds learned from the data when `constraints` sets no variabilities.
	#[serde(skip)]
	learner: LearnedSimilarity,
}

/// Similarity thresholds learned online, used when a dictionary has no variability
/// constraints.
///
/// Without them nothing is ever similar, so every batch became a new pattern
/// and a dictionary grew without bound (the fan daemon's `preamble` dictionaries).
///
/// Two shape distances are tracked: the mean absolute pointwise difference (what
/// `AverageStatic` checks) and the difference of maxima (`MaximumStatic`). For each, the
/// threshold is `λ ·` the median of recent nearest-neighbour distances, with one shared
/// radius `λ`. `λ` steers toward about [`TARGET_NEW_RATE`] new patterns per import in
/// bounded steps and is pulled back toward [`LAMBDA_CENTER`], so the pattern count is bounded by how many
/// λ-balls pack the data rather than by a cap. Every occurrence is still kept, and a
/// pattern within threshold of several stored ones still merges into all of them.
#[derive(Clone, Debug, Default)]
pub struct LearnedSimilarity {
	lambda: Option<f64>,
	recent: [std::collections::VecDeque<f64>; 2],
	window_imports: u32,
	window_new: u32,
	warned_at: usize,
	/// The learned step count, frozen once [`STEP_CALIBRATION_SAMPLES`] shapes are seen.
	steps: Option<usize>,
	calibration: Vec<Vec<f64>>,
}

/// The new-pattern rate the learned radius aims for.
pub const TARGET_NEW_RATE: f64 = 0.02;
const RECENT_DISTANCES: usize = 256;
const RATE_WINDOW: u32 = 50;
const LAMBDA_STEP: f64 = 1.25;
/// Where λ starts and is pulled back to.
///
/// A new shape merges unless it is more than twice as far from its nearest stored shape
/// as is typical. At 1 a shape exactly one typical
/// spacing away sits on the boundary, and evenly spread data never merges.
pub const LAMBDA_CENTER: f64 = 2.0;
const LAMBDA_BOUNDS: (f64, f64) = (0.05, 20.0);
const PATTERN_WARN_EVERY: usize = 10_000;
/// Shapes observed before the step count is chosen and frozen.
pub const STEP_CALIBRATION_SAMPLES: usize = 64;
/// Largest round-trip resampling error, as a fraction of a shape's range, that the
/// learned step count may introduce for 90% of the calibration shapes.
pub const STEP_RESAMPLE_TOLERANCE: f64 = 0.02;
const STEP_SEARCH_LIMIT: usize = 1024;

impl LearnedSimilarity {
	/// The current shared radius.
	#[must_use]
	pub fn lambda(&self) -> f64 {
		self.lambda.unwrap_or(LAMBDA_CENTER)
	}

	/// The current thresholds (average distance, maximum distance), or `None` before any
	/// nearest-neighbour distance has been seen.
	#[must_use]
	pub fn thresholds(&self) -> Option<[f64; 2]> {
		let m0 = median(&self.recent[0])?;
		let m1 = median(&self.recent[1])?;
		Some([self.lambda() * m0, self.lambda() * m1])
	}

	/// The learned step count, once frozen.
	#[must_use]
	pub const fn steps(&self) -> Option<usize> {
		self.steps
	}

	/// Collect one calibration shape; returns the step count when this one freezes it.
	fn calibrate(&mut self, shape: Vec<f64>) -> Option<usize> {
		if self.steps.is_some() {
			return None;
		}
		if shape.len() >= 2 {
			self.calibration.push(shape);
		}
		if self.calibration.len() < STEP_CALIBRATION_SAMPLES {
			return None;
		}
		self.steps = choose_steps(&self.calibration);
		self.calibration = Vec::new();
		self.steps
	}

	fn observe(&mut self, nearest: [f64; 2]) {
		for (k, d) in nearest.into_iter().enumerate() {
			// An exact repeat (distance 0) says nothing about the data's scale; counting it
			// would drag the median, and so the threshold, to 0, after which only exact
			// repeats merge and every distinct input becomes its own pattern.
			if d.is_finite() && d > 0.0 {
				let q = &mut self.recent[k];
				if q.len() == RECENT_DISTANCES {
					q.pop_front();
				}
				q.push_back(d);
			}
		}
	}

	fn record_outcome(&mut self, was_new: bool, pattern_count: usize) {
		self.window_imports += 1;
		self.window_new += u32::from(was_new);
		if self.window_imports >= RATE_WINDOW {
			let rate = f64::from(self.window_new) / f64::from(self.window_imports);
			let mut lambda = self.lambda();
			if rate > TARGET_NEW_RATE {
				lambda *= LAMBDA_STEP;
			} else if rate < TARGET_NEW_RATE {
				lambda /= LAMBDA_STEP;
			}
			// Pull back toward the center so one long quiet or noisy stretch cannot run it away.
			lambda = LAMBDA_CENTER * (lambda / LAMBDA_CENTER).powf(0.9);
			self.lambda = Some(lambda.clamp(LAMBDA_BOUNDS.0, LAMBDA_BOUNDS.1));
			self.window_imports = 0;
			self.window_new = 0;
		}
		if was_new && pattern_count >= self.warned_at + PATTERN_WARN_EVERY {
			self.warned_at = pattern_count;
			tracing::warn!(pattern_count, lambda = self.lambda(), "Dictionary keeps adding patterns under learned thresholds; the data may need explicit variability constraints");
		}
	}
}

fn median(values: &std::collections::VecDeque<f64>) -> Option<f64> {
	if values.is_empty() {
		return None;
	}
	let mut sorted: Vec<f64> = values.iter().copied().collect();
	sorted.sort_by(f64::total_cmp);
	Some(sorted[sorted.len() / 2])
}

/// Linear resampling of `s` to `n` evenly spaced points (the interpolation the step
/// conversion uses).
fn resample(s: &[f64], n: usize) -> Vec<f64> {
	#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	(0..n).map(|i| {
		if n == 1 || s.len() == 1 {
			return s[0];
		}
		let x = i as f64 * (s.len() - 1) as f64 / (n - 1) as f64;
		let lo = (x.floor() as usize).min(s.len() - 1);
		let hi = (lo + 1).min(s.len() - 1);
		(s[hi] - s[lo]).mul_add(x - lo as f64, s[lo])
	})
	.collect()
}

/// How much of `s` is lost by storing it at `steps` points: the largest pointwise error
/// after resampling down and back, as a fraction of the shape's range.
fn resample_loss(s: &[f64], steps: usize) -> f64 {
	let range = s.iter().copied().fold(f64::NEG_INFINITY, f64::max) - s.iter().copied().fold(f64::INFINITY, f64::min);
	if range.is_nan() || range <= 0.0 {
		return 0.0;
	}
	let back = resample(&resample(s, steps), s.len());
	s.iter().zip(&back).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max) / range
}

/// The smallest step count that keeps 90% of `samples` within
/// [`STEP_RESAMPLE_TOLERANCE`] of themselves after resampling. Step count and
/// interpolation are chosen together: the loss is measured under the interpolation the
/// conversion applies, so a count is only accepted if that interpolation reconstructs
/// the shapes at it.
fn choose_steps(samples: &[Vec<f64>]) -> Option<usize> {
	let longest = samples.iter().map(Vec::len).max()?.min(STEP_SEARCH_LIMIT);
	for steps in 2..=longest {
		let mut losses: Vec<f64> = samples.iter().map(|s| resample_loss(s, steps)).collect();
		losses.sort_by(f64::total_cmp);
		if losses[(losses.len() * 9 / 10).min(losses.len() - 1)] <= STEP_RESAMPLE_TOLERANCE {
			return Some(steps);
		}
	}
	Some(longest)
}

fn amplitudes_f64(pattern: &Pattern) -> Vec<f64> {
	pattern.amplitudes().iter().filter_map(ToPrimitive::to_f64).collect()
}

/// (mean absolute pointwise difference, |max − max|), or `None` when the shapes differ in length.
fn shape_distances(a: &[f64], b: &[f64]) -> Option<[f64; 2]> {
	if a.is_empty() || a.len() != b.len() {
		return None;
	}
	#[allow(clippy::cast_precision_loss)]
	let mean = a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f64>() / a.len() as f64;
	let max = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
	Some([mean, (max(a) - max(b)).abs()])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DictionaryMetadata {
	pub id: DictionaryId,
	pub name: String,
	pub description: String,
	pub constraints: DictionaryConstraints,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DictionaryConstraints {
	steps: Option<Steps>,
	variabilities: Option<Vec<VariablilityType>>,
}

impl DictionaryConstraints {
	/// Create new `DictionaryConstraints`
	#[must_use]
	pub const fn new(steps: Option<Steps>, variabilities: Option<Vec<VariablilityType>>) -> Self {
		Self { steps, variabilities }
	}

	/// Get the steps configuration
	#[must_use]
	pub const fn steps(&self) -> &Option<Steps> {
		&self.steps
	}

	/// Set the steps configuration
	pub const fn set_steps(&mut self, steps: Option<Steps>) {
		self.steps = steps;
	}

	/// Get the variabilities configuration
	#[must_use]
	pub const fn variabilities(&self) -> &Option<Vec<VariablilityType>> {
		&self.variabilities
	}

	/// Set the variabilities configuration
	pub fn set_variabilities(&mut self, variabilities: Option<Vec<VariablilityType>>) {
		self.variabilities = variabilities;
	}
}

impl Default for DictionaryConstraints {
	fn default() -> Self {
		Self::new(None, None)
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Steps {
	count: usize,
	interpolation: Spline,
}

impl Steps {
	/// Create new Steps configuration
	#[must_use]
	pub const fn new(count: usize, interpolation: Spline) -> Self {
		Self { count, interpolation }
	}

	/// Get the step count
	#[must_use]
	pub const fn count(&self) -> usize {
		self.count
	}

	/// Set the step count
	pub const fn set_count(&mut self, count: usize) {
		self.count = count;
	}

	/// Get the interpolation method
	#[must_use]
	pub const fn interpolation(&self) -> &Spline {
		&self.interpolation
	}

	/// Set the interpolation method
	pub const fn set_interpolation(&mut self, interpolation: Spline) {
		self.interpolation = interpolation;
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VariablilityType {
	MaximumStatic(Variability),
	AverageStatic(Variability),
	AbsoluteMaximumStatic(Variability),
	AbsoluteAverageStatic(Variability),

	MaximumPercentile(Variability),
	AveragePercentile(Variability),
	AbsoluteMaximumPercentile(Variability),
	AbsoluteAveragePercentile(Variability),

	SumStatic(Variability),
	SumPercentile(Variability),
	AbsoluteSumStatic(Variability),
	AbsoluteSumPercentile(Variability),
}

impl VariablilityType {
	/// The variant's name, e.g. `"AveragePercentile"`: what [`Display`] writes before the
	/// value, and what a dictionary stores in `dictionary_variabilities.variability_type`.
	pub(crate) const fn kind(&self) -> &'static str {
		match self {
			Self::MaximumStatic(_) => "MaximumStatic",
			Self::AverageStatic(_) => "AverageStatic",
			Self::AbsoluteMaximumStatic(_) => "AbsoluteMaximumStatic",
			Self::AbsoluteAverageStatic(_) => "AbsoluteAverageStatic",
			Self::MaximumPercentile(_) => "MaximumPercentile",
			Self::AveragePercentile(_) => "AveragePercentile",
			Self::AbsoluteMaximumPercentile(_) => "AbsoluteMaximumPercentile",
			Self::AbsoluteAveragePercentile(_) => "AbsoluteAveragePercentile",
			Self::SumStatic(_) => "SumStatic",
			Self::SumPercentile(_) => "SumPercentile",
			Self::AbsoluteSumStatic(_) => "AbsoluteSumStatic",
			Self::AbsoluteSumPercentile(_) => "AbsoluteSumPercentile",
		}
	}

	/// The variability every variant carries.
	pub(crate) const fn variability(&self) -> &Variability {
		match self {
			Self::MaximumStatic(v) | Self::AverageStatic(v) | Self::AbsoluteMaximumStatic(v) | Self::AbsoluteAverageStatic(v) | Self::MaximumPercentile(v) | Self::AveragePercentile(v) | Self::AbsoluteMaximumPercentile(v) | Self::AbsoluteAveragePercentile(v) | Self::SumStatic(v) | Self::SumPercentile(v) | Self::AbsoluteSumStatic(v) | Self::AbsoluteSumPercentile(v) => v,
		}
	}

	/// The variant [`kind`](Self::kind) names, carrying `variability`.
	///
	/// # Errors
	///
	/// `Unknown VariabilityType: <kind>` when `kind` names no variant.
	pub(crate) fn from_kind(kind: &str, variability: Variability) -> Result<Self> {
		Ok(match kind {
			"MaximumStatic" => Self::MaximumStatic(variability),
			"AverageStatic" => Self::AverageStatic(variability),
			"AbsoluteMaximumStatic" => Self::AbsoluteMaximumStatic(variability),
			"AbsoluteAverageStatic" => Self::AbsoluteAverageStatic(variability),
			"MaximumPercentile" => Self::MaximumPercentile(variability),
			"AveragePercentile" => Self::AveragePercentile(variability),
			"AbsoluteMaximumPercentile" => Self::AbsoluteMaximumPercentile(variability),
			"AbsoluteAveragePercentile" => Self::AbsoluteAveragePercentile(variability),
			"SumStatic" => Self::SumStatic(variability),
			"SumPercentile" => Self::SumPercentile(variability),
			"AbsoluteSumStatic" => Self::AbsoluteSumStatic(variability),
			"AbsoluteSumPercentile" => Self::AbsoluteSumPercentile(variability),
			_ => bail!("Unknown VariabilityType: {kind}"),
		})
	}
}

impl Display for VariablilityType {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}({})", self.kind(), self.variability().value())
	}
}

impl FromStr for VariablilityType {
	type Err = anyhow::Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let parts: Vec<&str> = s.trim_end_matches(')').split('(').collect();
		if parts.len() != 2 {
			bail!("Invalid VariabilityType format");
		}

		let var_type = parts[0];
		let value_str = parts[1];
		let value = BigDecimal::from_str(value_str)?;

		Self::from_kind(var_type, Variability::new(value))
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variability {
	value: BigDecimal,
}

impl Variability {
	/// Create new Variability with the given value
	#[must_use]
	pub const fn new(value: BigDecimal) -> Self {
		Self { value }
	}

	/// Get the variability value
	#[must_use]
	pub const fn value(&self) -> &BigDecimal {
		&self.value
	}

	/// Set the variability value
	pub fn set_value(&mut self, value: BigDecimal) {
		self.value = value;
	}
}

impl Dictionary {
	/// Create a new dictionary with the given constraints
	#[must_use]
	pub fn new(name: String, description: String, constraints: DictionaryConstraints) -> Self {
		let id = DictionaryId::new();
		Self { id, name, description, patterns: Vec::new(), constraints, learner: LearnedSimilarity::default() }
	}

	/// Get the dictionary ID
	#[must_use]
	pub const fn id(&self) -> &DictionaryId {
		&self.id
	}

	/// Get the dictionary name
	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Set the dictionary name
	pub fn set_name(&mut self, name: String) {
		self.name = name;
	}

	/// Get the dictionary description
	#[must_use]
	pub fn description(&self) -> &str {
		&self.description
	}

	/// Set the dictionary description
	pub fn set_description(&mut self, description: String) {
		self.description = description;
	}

	/// Get the patterns in the dictionary
	#[must_use]
	pub fn patterns(&self) -> &[Pattern] {
		&self.patterns
	}

	/// Get mutable access to patterns (for internal use)
	pub const fn patterns_mut(&mut self) -> &mut Vec<Pattern> {
		&mut self.patterns
	}

	/// Get the dictionary constraints
	#[must_use]
	pub const fn constraints(&self) -> &DictionaryConstraints {
		&self.constraints
	}

	/// Set the dictionary constraints
	pub fn set_constraints(&mut self, constraints: DictionaryConstraints) {
		self.constraints = constraints;
	}

	/// Put stored patterns back exactly as they were saved: no step conversion, no
	/// similarity matching, no merging. A pattern whose id is already present is skipped,
	/// so restoring twice does not duplicate. This is how a dictionary is reloaded; new
	/// data goes through [`import_pattern`](Self::import_pattern).
	pub fn restore_patterns(&mut self, patterns: Vec<Pattern>) {
		let mut seen: std::collections::HashSet<crate::PatternID> = self.patterns.iter().map(Pattern::id).collect();
		for pattern in patterns {
			if seen.insert(pattern.id()) {
				self.patterns.push(pattern);
			}
		}
		// A reloaded dictionary whose patterns all share one length learned that step count.
		if self.constraints.steps.is_none() && self.learner.steps.is_none() {
			let mut lengths = self.patterns.iter().map(|p| p.relatives().len()).filter(|l| *l > 0);
			if let Some(first) = lengths.next() {
				if self.patterns.len() >= STEP_CALIBRATION_SAMPLES && lengths.all(|l| l == first) {
					self.learner.steps = Some(first);
				}
			}
		}
		self.seed_learner();
	}

	/// The learned-threshold state (meaningful only without variability constraints).
	#[must_use]
	pub const fn learned_similarity(&self) -> &LearnedSimilarity {
		&self.learner
	}

	/// Re-seed the learned distances from up to [`RECENT_DISTANCES`] restored patterns, so a
	/// reloaded dictionary does not start from "nothing is similar" again. λ is kept.
	fn seed_learner(&mut self) {
		if self.constraints.variabilities.is_some() || !self.learner.recent[0].is_empty() {
			return;
		}
		let shapes: Vec<Vec<f64>> = self.patterns.iter().map(amplitudes_f64).collect();
		let step = (shapes.len() / RECENT_DISTANCES).max(1);
		for i in (0..shapes.len()).step_by(step).take(RECENT_DISTANCES) {
			let mut nearest = [f64::INFINITY; 2];
			for (j, other) in shapes.iter().enumerate() {
				if i != j {
					if let Some(d) = shape_distances(&shapes[i], other) {
						nearest = [nearest[0].min(d[0]), nearest[1].min(d[1])];
					}
				}
			}
			self.learner.observe(nearest);
		}
	}

	/// Every stored pattern within the learned thresholds of `new_pattern` (merge-into-all),
	/// recording its nearest-neighbour distances. Before any distance is known nothing matches.
	fn learned_matches(&mut self, new_pattern: &Pattern) -> Vec<usize> {
		let shape = amplitudes_f64(new_pattern);
		let thresholds = self.learner.thresholds();
		let mut nearest = [f64::INFINITY; 2];
		let mut matches = Vec::new();
		for (idx, existing) in self.patterns.iter().enumerate() {
			let Some(d) = shape_distances(&shape, &amplitudes_f64(existing)) else { continue };
			nearest = [nearest[0].min(d[0]), nearest[1].min(d[1])];
			if thresholds.is_some_and(|t| d[0] <= t[0] && d[1] <= t[1]) {
				matches.push(idx);
			}
		}
		self.learner.observe(nearest);
		matches
	}

	/// Import a new pattern into the dictionary following the exact specification
	///
	/// ALL patterns are processed identically regardless of dictionary size:
	/// 1. Enforce steps constraint if configured
	/// 2. Check similarity against ALL existing patterns using configured variability constraints
	/// 3. If similar patterns found, merge occurrences; otherwise add as new pattern
	///
	/// # Errors
	///
	/// Returns an error if pattern processing fails due to step conversion or similarity checking.
	pub fn import_pattern(&mut self, mut new_pattern: Pattern) -> Result<()> {
		// Without a configured step count, learn one from the first shapes, then convert the
		// patterns stored so far to it, so shapes of different lengths become comparable.
		if self.constraints.steps.is_none() {
			if let Some(count) = self.learner.calibrate(amplitudes_f64(&new_pattern)) {
				let steps = Steps::new(count, Spline::Linear);
				tracing::info!(steps = count, dictionary = %self.name, "Learned the dictionary step count");
				self.patterns = std::mem::take(&mut self.patterns).into_iter().map(|p| Self::convert_pattern_steps(p, &steps)).collect::<Result<_>>()?;
			}
		}
		let learned_steps = self.learner.steps.map(|count| Steps::new(count, Spline::Linear));

		// Step 1: Enforce steps constraint if configured (or learned)
		if let Some(steps_config) = self.constraints.steps.as_ref().or(learned_steps.as_ref()) {
			new_pattern = Self::convert_pattern_steps(new_pattern, steps_config)?;

			// Debug: Check after step conversion
			if new_pattern.relatives().is_empty() {
				return Ok(());
			}
		}

		// A pattern already in the dictionary (by id) is not added again: loading a store
		// into a dictionary that already holds it must not duplicate it.
		if self.patterns.iter().any(|p| p.id() == new_pattern.id()) {
			return Ok(());
		}

		// Step 2: Check similarity against ALL existing patterns using configured variability
		// constraints, or, when none are configured, the learned thresholds.
		let matching_indices = if self.constraints.variabilities.is_none() {
			self.learned_matches(&new_pattern)
		} else {
			let mut found = Vec::new();
			for (idx, existing_pattern) in self.patterns.iter().enumerate() {
				if self.patterns_are_similar(&new_pattern, existing_pattern)? {
					found.push(idx);
				}
			}
			found
		};

		// Step 3: Handle matches according to specification
		if matching_indices.is_empty() {
			// "If the new pattern is not found to be efficiently similar to any existing pattern
			// then the new pattern will be added to the pattern dictionary"
			self.patterns.push(new_pattern);
			if self.constraints.variabilities.is_none() {
				self.learner.record_outcome(true, self.patterns.len());
			}
		} else {
			if self.constraints.variabilities.is_none() {
				self.learner.record_outcome(false, self.patterns.len());
			}
			// "If they are deemed sufficiently similar the new pattern will be converted to an
			// occurrence of the existing pattern" - merge with ALL matching patterns as specified
			for &idx in &matching_indices {
				self.merge_pattern_occurrences_at_index(idx, &new_pattern)?;
			}
		}

		Ok(())
	}

	/// Convert pattern to match the required number of steps using direct interpolation
	///
	/// # Errors
	///
	/// Returns an error if pattern conversion fails due to invalid step configuration or interpolation issues.
	fn convert_pattern_steps(pattern: Pattern, steps_config: &Steps) -> Result<Pattern> {
		let relatives = pattern.relatives();
		if relatives.is_empty() {
			return Ok(pattern);
		}

		let current_steps = relatives.len();
		let required_steps = steps_config.count;

		if required_steps == 0 {
			bail!("Steps count must be greater than 0");
		}

		if current_steps == required_steps {
			return Ok(pattern);
		}

		let denominator = required_steps.saturating_sub(1);

		// If we only have one point, duplicate it across all required steps
		if relatives.len() == 1 {
			let single_relative = &relatives[0];
			let mut new_relatives = Vec::with_capacity(required_steps);
			for i in 0..required_steps {
				let ratio = if denominator == 0 { 0.0 } else { safe_ratio(i, denominator).map_err(anyhow::Error::from)? };
				let location = BigDecimal::from_f64(ratio).ok_or_else(|| crate::Error::NumericConversionError(format!("Failed to convert ratio {ratio} to BigDecimal")))?;
				let vector = MeasurementVector::new(location, single_relative.vector().amplitude().clone());
				new_relatives.push(Relative::new(vector, single_relative.max_x().clone(), single_relative.max_y().clone()));
			}

			return Ok(Pattern::with_log(pattern.id(), pattern.occurrence_log().clone(), new_relatives));
		}

		let avg_max_x = Self::calculate_average_max_x(relatives);
		let avg_max_y = Self::calculate_average_max_y(relatives);
		let mut new_relatives = Vec::with_capacity(required_steps);

		// Perform direct interpolation on the relative data
		for i in 0..required_steps {
			let target_location = if denominator == 0 {
				BigDecimal::from(0)
			} else {
				let ratio = safe_ratio(i, denominator).map_err(anyhow::Error::from)?;
				BigDecimal::from_f64(ratio).ok_or_else(|| crate::Error::NumericConversionError(format!("Failed to convert ratio {ratio} to BigDecimal")))?
			};

			let interpolated_amplitude = interpolate_amplitude_at_location(relatives, &target_location);
			let vector = MeasurementVector::new(target_location, interpolated_amplitude);
			new_relatives.push(Relative::new(vector, avg_max_x.clone(), avg_max_y.clone()));
		}

		Ok(Pattern::with_log(pattern.id(), pattern.occurrence_log().clone(), new_relatives))
	}

	/// Check if two patterns are similar based on the dictionary's variability constraints
	///
	/// # Errors
	///
	/// Returns an error if variability constraint checking fails.
	pub fn patterns_are_similar(&self, pattern1: &Pattern, pattern2: &Pattern) -> Result<bool> {
		if pattern1.amplitudes().len() != pattern2.amplitudes().len() {
			return Ok(false);
		}

		if let Some(variabilities) = &self.constraints.variabilities {
			// All variability constraints must be satisfied for patterns to be considered similar
			for variability in variabilities {
				if !Self::check_variability_constraint(pattern1, pattern2, variability)? {
					return Ok(false);
				}
			}
			Ok(true)
		} else {
			// No variability constraints defined - patterns are not similar by default
			Ok(false)
		}
	}

	/// Check a specific variability constraint between two patterns using SIMD optimization
	///
	/// # Errors
	///
	/// Returns an error if the variability constraint type is not supported or checking fails.
	fn check_variability_constraint(pattern1: &Pattern, pattern2: &Pattern, variability: &VariablilityType) -> Result<bool> {
		match variability {
			VariablilityType::MaximumStatic(static_var) => Ok(Self::check_maximum_static(pattern1, pattern2, &static_var.value)),
			VariablilityType::AverageStatic(static_var) => Self::check_average_static(pattern1, pattern2, &static_var.value),
			VariablilityType::AbsoluteMaximumStatic(static_var) => Ok(Self::check_absolute_maximum_static(pattern1, pattern2, &static_var.value)),
			VariablilityType::AbsoluteAverageStatic(static_var) => Self::check_absolute_average_static(pattern1, pattern2, &static_var.value),
			VariablilityType::MaximumPercentile(percentile_var) => Ok(Self::check_maximum_percentile(pattern1, pattern2, &percentile_var.value)),
			VariablilityType::AveragePercentile(percentile_var) => Ok(Self::check_average_percentile(pattern1, pattern2, &percentile_var.value)),
			VariablilityType::AbsoluteMaximumPercentile(percentile_var) => Ok(Self::check_absolute_maximum_percentile(pattern1, pattern2, &percentile_var.value)),
			VariablilityType::AbsoluteAveragePercentile(percentile_var) => Ok(Self::check_absolute_average_percentile(pattern1, pattern2, &percentile_var.value)),
			VariablilityType::SumStatic(static_var) => Ok(Self::check_sum_static(pattern1, pattern2, &static_var.value)),
			VariablilityType::SumPercentile(static_var) => Ok(Self::check_sum_percentile(pattern1, pattern2, &static_var.value)),
			VariablilityType::AbsoluteSumStatic(static_var) => Ok(Self::check_absolute_sum_static(pattern1, pattern2, &static_var.value)),
			VariablilityType::AbsoluteSumPercentile(static_var) => Ok(Self::check_absolute_sum_percentile(pattern1, pattern2, &static_var.value)),
		}
	}

	/// Check absolute sum percentile variability
	fn check_absolute_sum_percentile(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		// Handle empty patterns
		if pattern1.amplitudes().is_empty() || pattern2.amplitudes().is_empty() {
			return true; // Empty patterns are considered similar
		}

		let sum1 = pattern1.abs_sum();
		let sum2 = pattern2.abs_sum();
		let diff = (sum1 - sum2).abs();
		let percentile = if sum2.is_zero() {
			if diff.is_zero() {
				BigDecimal::zero()
			} else {
				BigDecimal::from(10000) // Fixed: very large percentage for infinite difference
			}
		} else {
			(&diff / sum2) * BigDecimal::from(100)
		};
		percentile <= *threshold
	}

	/// Check sum percentile variability
	fn check_sum_percentile(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		// Handle empty patterns
		if pattern1.amplitudes().is_empty() || pattern2.amplitudes().is_empty() {
			return true; // Empty patterns are considered similar
		}

		let sum1 = pattern1.sum();
		let sum2 = pattern2.sum();
		let diff = (sum1 - sum2).abs();
		let percentile = if sum2.is_zero() {
			if diff.is_zero() {
				BigDecimal::zero()
			} else {
				BigDecimal::from(10000) // Fixed: very large percentage for infinite difference
			}
		} else {
			(&diff / sum2) * BigDecimal::from(100)
		};
		percentile <= *threshold
	}

	/// Check maximum percentile variability
	fn check_maximum_percentile(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let max_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1 - a2).abs();
				if a2.is_zero() {
					if diff.is_zero() {
						BigDecimal::zero()
					} else {
						BigDecimal::from(10000) // Fixed: very large percentage for infinite difference
					}
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
			.unwrap_or_else(BigDecimal::zero);
		max_percent <= *threshold
	}

	/// Check average percentile variability
	fn check_average_percentile(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1 - a2).abs();

				// Debug: print first few comparisons (disabled for performance)
				// if i < 3 {
				//     println!("  DEBUG AveragePercentile[{}]: a1={}, a2={}, diff={}, percent={}%", i, a1, a2, &diff, &percent);
				// }

				if a2.is_zero() {
					// When denominator is zero, percentage difference should be infinite if numerator is non-zero
					// For zero differences, treat as 0% difference
					if diff.is_zero() {
						BigDecimal::zero()
					} else {
						// Return a very large percentage to indicate significant difference
						BigDecimal::from(10000) // 10000% - effectively infinite for our purposes
					}
				} else {
					(&diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.fold(BigDecimal::zero(), |acc, p| acc + p);
		let avg_percent = sum_percent / len;

		// Debug output (disabled for performance)
		// static COMPARISON_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
		// let count = COMPARISON_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		// if count < 5 {
		//     println!("  -> COMPARISON {}: Average percentile: {}%, threshold: {}%, similar: {}", count, avg_percent, threshold, is_similar);
		// }

		avg_percent <= *threshold
	}

	/// Check absolute maximum percentile variability
	fn check_absolute_maximum_percentile(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let max_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1.abs() - a2.abs()).abs();
				if a2.is_zero() {
					if diff.is_zero() {
						BigDecimal::zero()
					} else {
						BigDecimal::from(10000) // Fixed: very large percentage for infinite difference
					}
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
			.unwrap_or_else(BigDecimal::zero);
		max_percent <= *threshold
	}

	/// Check absolute average percentile variability
	fn check_absolute_average_percentile(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		// Handle empty patterns
		if pattern1.amplitudes().is_empty() || pattern2.amplitudes().is_empty() {
			return true; // Empty patterns are considered similar
		}

		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1.abs() - a2.abs()).abs();
				if a2.abs().is_zero() {
					if diff.is_zero() {
						BigDecimal::zero()
					} else {
						BigDecimal::from(10000) // Fixed: very large percentage for infinite difference
					}
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.fold(BigDecimal::zero(), |acc, p| acc + p);
		let avg_percent = sum_percent / len;

		avg_percent <= *threshold
	}

	/// Merge occurrences from one pattern into another at a specific index
	///
	/// # Errors
	///
	/// This function does not return an error - it always succeeds.
	pub fn merge_pattern_occurrences_at_index(&mut self, target_index: usize, source_pattern: &Pattern) -> Result<()> {
		if let Some(target_pattern) = self.patterns.get_mut(target_index) {
			// Add all occurrences from source pattern to target pattern
			for occurrence in source_pattern.occurrence_log().iter() {
				target_pattern.add_occurrence(occurrence);
			}
		}
		Ok(())
	}

	/// Calculate average `max_x` from relatives
	fn calculate_average_max_x(relatives: &[Relative]) -> BigDecimal {
		if relatives.is_empty() {
			return BigDecimal::from(1);
		}

		let sum = relatives.iter().fold(BigDecimal::from(0), |acc, rel| acc + rel.max_x());
		let len = BigDecimal::from(i64::try_from(relatives.len()).unwrap_or(1));
		sum / len
	}

	/// Calculate average `max_y` from relatives
	fn calculate_average_max_y(relatives: &[Relative]) -> BigDecimal {
		if relatives.is_empty() {
			return BigDecimal::from(1);
		}

		let sum = relatives.iter().fold(BigDecimal::from(0), |acc, rel| acc + rel.max_y());
		let len = BigDecimal::from(i64::try_from(relatives.len()).unwrap_or(1));
		sum / len
	}

	#[must_use]
	pub const fn len(&self) -> usize {
		self.patterns.len()
	}

	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.patterns.is_empty()
	}

	// SIMD-optimized constraint checking methods for better performance

	/// SIMD-optimized maximum static constraint checking
	fn check_maximum_static(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return false;
		}

		let max1 = amps1.iter().copied().fold(f64::NEG_INFINITY, f64::max);
		let max2 = amps2.iter().copied().fold(f64::NEG_INFINITY, f64::max);

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		(max1 - max2).abs() <= threshold_f64
	}

	/// SIMD-optimized average static constraint checking
	fn check_average_static(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return Ok(false);
		}

		let mut sum_diff = 0.0f64;

		// Process 4 differences at a time with SIMD
		let chunks = amps1.len() / 4;
		for i in 0..chunks {
			let start = i * 4;
			if start + 3 < amps1.len() {
				let v1 = f64x4::new([amps1[start], amps1[start + 1], amps1[start + 2], amps1[start + 3]]);
				let v2 = f64x4::new([amps2[start], amps2[start + 1], amps2[start + 2], amps2[start + 3]]);

				let diff = (v1 - v2).abs();
				sum_diff += diff.reduce_add();
			}
		}

		// Handle remaining elements
		for i in (chunks * 4)..amps1.len() {
			sum_diff += (amps1[i] - amps2[i]).abs();
		}

		let avg_diff = sum_diff / safe_usize_to_f64(amps1.len()).map_err(anyhow::Error::from)?;
		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		Ok(avg_diff <= threshold_f64)
	}

	/// SIMD-optimized absolute maximum static constraint checking
	fn check_absolute_maximum_static(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return false;
		}

		// Use SIMD to find absolute maximum in chunks
		let mut abs_max1 = 0.0f64;
		let mut abs_max2 = 0.0f64;

		let chunks = amps1.len() / 4;
		for i in 0..chunks {
			let start = i * 4;
			if start + 3 < amps1.len() {
				let v1 = f64x4::new([amps1[start], amps1[start + 1], amps1[start + 2], amps1[start + 3]]);
				let v2 = f64x4::new([amps2[start], amps2[start + 1], amps2[start + 2], amps2[start + 3]]);

				let abs_v1 = v1.abs();
				let abs_v2 = v2.abs();

				// Extract individual values to find max (since reduce_max doesn't exist)
				let values1 = abs_v1.to_array();
				let values2 = abs_v2.to_array();

				for &val in &values1 {
					abs_max1 = abs_max1.max(val);
				}
				for &val in &values2 {
					abs_max2 = abs_max2.max(val);
				}
			}
		}

		// Handle remaining elements
		for i in (chunks * 4)..amps1.len() {
			abs_max1 = abs_max1.max(amps1[i].abs());
			abs_max2 = abs_max2.max(amps2[i].abs());
		}

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		(abs_max1 - abs_max2).abs() <= threshold_f64
	}

	/// SIMD-optimized absolute average static constraint checking
	fn check_absolute_average_static(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return Ok(false);
		}

		let mut abs_sum1 = 0.0f64;
		let mut abs_sum2 = 0.0f64;

		// Process 4 values at a time with SIMD
		let chunks = amps1.len() / 4;
		for i in 0..chunks {
			let start = i * 4;
			if start + 3 < amps1.len() {
				let v1 = f64x4::new([amps1[start], amps1[start + 1], amps1[start + 2], amps1[start + 3]]);
				let v2 = f64x4::new([amps2[start], amps2[start + 1], amps2[start + 2], amps2[start + 3]]);

				let abs_v1 = v1.abs();
				let abs_v2 = v2.abs();

				abs_sum1 += abs_v1.reduce_add();
				abs_sum2 += abs_v2.reduce_add();
			}
		}

		// Handle remaining elements
		for i in (chunks * 4)..amps1.len() {
			abs_sum1 += amps1[i].abs();
			abs_sum2 += amps2[i].abs();
		}

		let abs_avg1 = abs_sum1 / safe_usize_to_f64(amps1.len()).map_err(anyhow::Error::from)?;
		let abs_avg2 = abs_sum2 / safe_usize_to_f64(amps2.len()).map_err(anyhow::Error::from)?;

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		Ok((abs_avg1 - abs_avg2).abs() <= threshold_f64)
	}

	/// SIMD-optimized absolute sum static constraint checking
	fn check_absolute_sum_static(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return false;
		}

		let mut abs_sum1 = 0.0f64;
		let mut abs_sum2 = 0.0f64;

		// Process 4 values at a time with SIMD
		let chunks = amps1.len() / 4;
		for i in 0..chunks {
			let start = i * 4;
			if start + 3 < amps1.len() {
				let v1 = f64x4::new([amps1[start], amps1[start + 1], amps1[start + 2], amps1[start + 3]]);
				let v2 = f64x4::new([amps2[start], amps2[start + 1], amps2[start + 2], amps2[start + 3]]);

				let abs_v1 = v1.abs();
				let abs_v2 = v2.abs();

				abs_sum1 += abs_v1.reduce_add();
				abs_sum2 += abs_v2.reduce_add();
			}
		}

		// Handle remaining elements
		for i in (chunks * 4)..amps1.len() {
			abs_sum1 += amps1[i].abs();
			abs_sum2 += amps2[i].abs();
		}

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		(abs_sum1 - abs_sum2).abs() <= threshold_f64
	}

	/// SIMD-optimized sum static constraint checking
	fn check_sum_static(pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> bool {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(bigdecimal::ToPrimitive::to_f64).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return false;
		}

		let mut sum1 = 0.0f64;
		let mut sum2 = 0.0f64;

		// Process 4 values at a time with SIMD
		let chunks = amps1.len() / 4;
		for i in 0..chunks {
			let start = i * 4;
			if start + 3 < amps1.len() {
				let v1 = f64x4::new([amps1[start], amps1[start + 1], amps1[start + 2], amps1[start + 3]]);
				let v2 = f64x4::new([amps2[start], amps2[start + 1], amps2[start + 2], amps2[start + 3]]);

				sum1 += v1.reduce_add();
				sum2 += v2.reduce_add();
			}
		}

		// Handle remaining elements
		for i in (chunks * 4)..amps1.len() {
			sum1 += amps1[i];
			sum2 += amps2[i];
		}

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		(sum1 - sum2).abs() <= threshold_f64
	}

	/// Merge another dictionary into this one
	/// All patterns from the other dictionary are imported using the standard `import_pattern` logic
	/// This ensures all similarity constraints are respected during the merge
	///
	/// # Errors
	///
	/// Returns an error if any pattern import fails during the merge process.
	pub fn merge_dictionary(&mut self, other: Self) -> Result<()> {
		// Import each pattern from the other dictionary
		for pattern in other.patterns {
			self.import_pattern(pattern)?;
		}
		Ok(())
	}

	/// Serialize the dictionary to JSON string
	///
	/// # Errors
	///
	/// Returns an error if JSON serialization fails.
	pub fn to_json(&self) -> Result<String> {
		serde_json::to_string(self).map_err(|e| anyhow::anyhow!("Failed to serialize dictionary: {e}"))
	}

	/// Serialize the dictionary to JSON string with pretty formatting
	///
	/// # Errors
	///
	/// Returns an error if JSON serialization fails.
	pub fn to_json_pretty(&self) -> Result<String> {
		serde_json::to_string_pretty(self).map_err(|e| anyhow::anyhow!("Failed to serialize dictionary: {e}"))
	}

	/// Deserialize a dictionary from JSON string
	///
	/// # Errors
	///
	/// Returns an error if JSON deserialization fails or the JSON is invalid.
	pub fn from_json(json: &str) -> Result<Self> {
		serde_json::from_str(json).map_err(|e| anyhow::anyhow!("Failed to deserialize dictionary: {e}"))
	}

	/// Serialize the dictionary to binary format (using postcard)
	///
	/// # Errors
	///
	/// Returns an error if binary serialization fails.
	pub fn to_bytes(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(|e| anyhow::anyhow!("Failed to serialize dictionary to bytes: {e}"))
	}

	/// Deserialize a dictionary from binary format (using postcard)
	///
	/// # Errors
	///
	/// Returns an error if binary deserialization fails or the data is invalid.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		postcard::from_bytes(bytes).map_err(|e| anyhow::anyhow!("Failed to deserialize dictionary from bytes: {e}"))
	}
}

/// Helper function to interpolate amplitude at a specific location using linear interpolation
fn interpolate_amplitude_at_location(relatives: &[Relative], target_location: &BigDecimal) -> BigDecimal {
	if relatives.is_empty() {
		return BigDecimal::zero();
	}

	// If we only have one point, return its amplitude
	if relatives.len() == 1 {
		return relatives[0].vector().amplitude().clone();
	}

	// Convert target location to f64 for easier calculations
	let target_loc_f64 = target_location.to_f64().unwrap_or(0.0);

	// Find the two relatives that bracket our target location
	let mut left_idx = 0;
	let mut right_idx = relatives.len() - 1;

	for (i, relative) in relatives.iter().enumerate() {
		let loc_f64 = relative.vector().location().to_f64().unwrap_or(0.0);
		if loc_f64 <= target_loc_f64 {
			left_idx = i;
		}
		if loc_f64 >= target_loc_f64 && right_idx == relatives.len() - 1 {
			right_idx = i;
			break;
		}
	}

	// If target is exactly at a known point, return that amplitude
	if left_idx == right_idx {
		return relatives[left_idx].vector().amplitude().clone();
	}

	// Perform linear interpolation between left and right points
	let left_relative = &relatives[left_idx];
	let right_relative = &relatives[right_idx];

	let left_loc = left_relative.vector().location().to_f64().unwrap_or(0.0);
	let right_loc = right_relative.vector().location().to_f64().unwrap_or(0.0);
	let left_amp = left_relative.vector().amplitude().to_f64().unwrap_or(0.0);
	let right_amp = right_relative.vector().amplitude().to_f64().unwrap_or(0.0);

	// Handle edge case where locations are the same
	if (right_loc - left_loc).abs() < f64::EPSILON {
		return left_relative.vector().amplitude().clone();
	}

	// Linear interpolation: y = y1 + (y2 - y1) * (x - x1) / (x2 - x1)
	let interpolation_factor = (target_loc_f64 - left_loc) / (right_loc - left_loc);
	let interpolated_amplitude = (right_amp - left_amp).mul_add(interpolation_factor, left_amp);

	BigDecimal::from_f64(interpolated_amplitude).unwrap_or_else(BigDecimal::zero)
}

#[cfg(test)]
#[allow(clippy::significant_drop_tightening)]
mod tests {
	use bigdecimal::FromPrimitive;
	use chrono::Utc;
	use serial_test::serial;
	use splimes::Resolution;

	use super::*;
	use crate::{
		types::{MeasurementVector, Occurrence, PatternID, Relative}, AspectId, DatabaseInfo
	};

	fn create_test_pattern(amplitudes: Vec<f64>) -> Pattern {
		let pattern_id = PatternID::new();
		let occurrences = vec![Occurrence::new(AspectId::new(), Resolution::Seconds, amplitudes.len(), DatabaseInfo::new("test".to_string(), "test_path".to_string()), pattern_id, Utc::now(), Utc::now())];

		let relatives: Vec<Relative> = amplitudes
			.into_iter()
			.enumerate()
			.map(|(i, amp)| {
				let location_ratio = safe_usize_to_f64(i).expect("Failed to convert index to f64") / 10.0;
				let location = BigDecimal::from_f64(location_ratio).unwrap();
				let amplitude = BigDecimal::from_f64(amp).unwrap();
				let vector = MeasurementVector::new(location, amplitude);
				Relative::new(vector, BigDecimal::from(1), BigDecimal::from(1))
			})
			.collect();

		Pattern::new(pattern_id, occurrences, relatives)
	}

	fn create_test_dictionary() -> Dictionary {
		let constraints = DictionaryConstraints {
			steps: Some(Steps { count: 10, interpolation: Spline::Linear }),
			variabilities: Some(vec![VariablilityType::AbsoluteSumPercentile(Variability {
                                        value: BigDecimal::from_f64(10.0).unwrap(), // 10% threshold - stored as 10.0, not 0.1
                                })]),
		};

		Dictionary::new("Test Dictionary".to_string(), "A test dictionary for pattern matching".to_string(), constraints)
	}

	#[serial]
	#[tokio::test]
	async fn test_import_new_pattern() {
		let mut dictionary = create_test_dictionary();

		let result = dictionary.import_pattern(create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]));
		assert!(result.is_ok());
		assert_eq!(dictionary.patterns.len(), 1);
		drop(dictionary);
	}

	#[serial]
	#[tokio::test]
	async fn test_import_similar_pattern() {
		let mut dictionary = create_test_dictionary();

		// Import first pattern
		dictionary.import_pattern(create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0])).unwrap();

		// Import similar pattern (should merge with existing)
		dictionary.import_pattern(create_test_pattern(vec![1.1, 2.1, 3.1, 2.1, 1.1])).unwrap();

		// Should still have only one pattern, but with two occurrences
		assert_eq!(dictionary.patterns.len(), 1);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2);
		drop(dictionary);
	}

	#[serial]
	#[tokio::test]
	async fn test_import_different_pattern() {
		let mut dictionary = create_test_dictionary();

		// Import first pattern
		dictionary.import_pattern(create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0])).unwrap();

		// Import very different pattern
		dictionary.import_pattern(create_test_pattern(vec![10.0, 20.0, 30.0, 20.0, 10.0])).unwrap();

		// Should have two different patterns
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 1);
		assert_eq!(dictionary.patterns[1].occurrences().len(), 1);
		drop(dictionary);
	}

	#[serial]
	#[tokio::test]
	async fn test_import_pattern_matches_multiple() {
		let mut dictionary = create_test_dictionary();

		// Import first pattern
		dictionary.import_pattern(create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0])).unwrap();

		// Import second similar pattern
		dictionary.import_pattern(create_test_pattern(vec![1.05, 2.05, 3.05, 2.05, 1.05])).unwrap();

		// Both should be merged into one pattern due to similarity
		assert_eq!(dictionary.patterns.len(), 1);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2);

		// Now import a pattern that could match both existing similar patterns
		// This pattern should be added to the existing pattern (which already contains both similar occurrences)
		dictionary.import_pattern(create_test_pattern(vec![1.08, 2.08, 3.08, 2.08, 1.08])).unwrap();

		// Should still have one pattern, now with three occurrences
		assert_eq!(dictionary.patterns.len(), 1);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 3);
		drop(dictionary);
	}

	#[serial]
	#[tokio::test]
	async fn test_import_pattern_matches_multiple_distinct_patterns() {
		// Create a dictionary with moderately loose constraints to allow some matches
		let constraints = DictionaryConstraints {
			steps: Some(Steps { count: 10, interpolation: Spline::Linear }),
			variabilities: Some(vec![VariablilityType::AbsoluteSumPercentile(Variability {
				value: BigDecimal::from_f64(25.0).unwrap(), // 25% threshold
			})]),
		};
		let mut dictionary = Dictionary::new("Moderate Test Dictionary".to_string(), "A test dictionary with moderate constraints".to_string(), constraints);

		// Import first pattern - interpolated sum ≈ 13.56
		dictionary.import_pattern(create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0])).unwrap();

		// Import a significantly different pattern - interpolated sum ≈ 22.56
		dictionary.import_pattern(create_test_pattern(vec![3.0, 6.0, 10.0, 6.0, 0.0])).unwrap();

		// These should be separate patterns due to significant difference
		// After interpolation: |22.56 - 13.56|/13.56 = 66.4% > 25% threshold
		assert_eq!(dictionary.patterns.len(), 2);

		// Now import a pattern that matches the first but not the second
		// Interpolated sum ≈ 16.27: |16.27 - 13.56|/13.56 = 20% < 25% (should match)
		// But not within 25% of 22.56: |22.56 - 16.27|/16.27 = 38.6% > 25%
		dictionary.import_pattern(create_test_pattern(vec![1.2, 2.4, 3.6, 2.4, 1.2])).unwrap();

		// Should still have 2 patterns, first should have 2 occurrences, second should have 1
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2); // pattern1 + pattern3
		assert_eq!(dictionary.patterns[1].occurrences().len(), 1); // pattern2 only
		drop(dictionary);
	}

	#[serial]
	#[tokio::test]
	async fn test_import_pattern_matches_multiple_existing_patterns() {
		// Create a dictionary with loose constraints to enable multiple matches
		let constraints = DictionaryConstraints {
			steps: Some(Steps { count: 3, interpolation: Spline::Linear }),
			variabilities: Some(vec![VariablilityType::AbsoluteSumPercentile(Variability {
				value: BigDecimal::from_f64(25.0).unwrap(), // 25% threshold - moderate
			})]),
		};
		let mut dictionary = Dictionary::new("Multi-Match Test Dictionary".to_string(), "Test dictionary for multiple pattern matching".to_string(), constraints);

		// Import first pattern - sum = 6.0
		dictionary.import_pattern(create_test_pattern(vec![2.0, 2.0, 2.0])).unwrap();

		// Import second pattern - sum = 8.0 (should remain separate)
		// Difference: |8.0 - 6.0|/6.0 = 33.3% > 25%, so these remain separate
		dictionary.import_pattern(create_test_pattern(vec![3.0, 2.0, 3.0])).unwrap();

		// Verify they're separate
		assert_eq!(dictionary.patterns.len(), 2);

		// Now import a pattern that matches both
		// Sum = 7.0
		// vs pattern1 (sum 6.0): |7.0 - 6.0|/6.0 = 16.7% < 25% ✓
		// vs pattern2 (sum 8.0): |7.0 - 8.0|/8.0 = 12.5% < 25% ✓
		dictionary.import_pattern(create_test_pattern(vec![2.5, 2.0, 2.5])).unwrap();

		// Should still have 2 patterns, but BOTH should now have 2 occurrences
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2); // pattern1 + pattern3
		assert_eq!(dictionary.patterns[1].occurrences().len(), 2); // pattern2 + pattern3
		drop(dictionary);
	}

	#[test]
	fn variability_types_round_trip_through_their_kind_and_text() {
		let value = || Variability::new(BigDecimal::from_str("0.000000001").unwrap());
		let all = [VariablilityType::MaximumStatic(value()), VariablilityType::AverageStatic(value()), VariablilityType::AbsoluteMaximumStatic(value()), VariablilityType::AbsoluteAverageStatic(value()), VariablilityType::MaximumPercentile(value()), VariablilityType::AveragePercentile(value()), VariablilityType::AbsoluteMaximumPercentile(value()), VariablilityType::AbsoluteAveragePercentile(value()), VariablilityType::SumStatic(value()), VariablilityType::SumPercentile(value()), VariablilityType::AbsoluteSumStatic(value()), VariablilityType::AbsoluteSumPercentile(value())];
		for variability in all {
			// The (kind, value) pair a dictionary stores, and the text `Display` writes.
			let stored = VariablilityType::from_kind(variability.kind(), variability.variability().clone()).unwrap();
			let parsed: VariablilityType = variability.to_string().parse().unwrap();
			for other in [stored, parsed] {
				assert_eq!((other.kind(), other.variability().value()), (variability.kind(), variability.variability().value()));
			}
		}
		assert_eq!(VariablilityType::from_kind("MedianStatic", value()).unwrap_err().to_string(), "Unknown VariabilityType: MedianStatic");
	}

	#[test]
	fn learned_thresholds_bound_a_repeating_signal_and_keep_every_occurrence() -> Result<()> {
		let mut dictionary = Dictionary::new("learned".into(), String::new(), DictionaryConstraints::new(None, None));
		// Three recurring shapes plus small deterministic noise, 3000 imports.
		let shapes = [[1.0, 2.0, 3.0, 2.0], [5.0, 5.0, 1.0, 0.0], [0.0, 4.0, 0.0, 4.0]];
		for i in 0..3000_u32 {
			let noise = f64::from((i * 7919) % 101) / 1000.0;
			let shape: Vec<f64> = shapes[(i % 3) as usize].iter().map(|v| v + noise).collect();
			dictionary.import_pattern(create_test_pattern(shape))?;
		}
		let occurrences: usize = dictionary.patterns().iter().map(|p| p.occurrences().len()).sum();
		assert!(dictionary.len() < 300, "{} patterns for 3 recurring shapes", dictionary.len());
		assert!(occurrences >= 3000, "every import is kept as an occurrence ({occurrences})");
		Ok(())
	}

	#[test]
	fn a_step_count_is_learned_from_the_first_shapes_and_frozen() {
		let mut dictionary = Dictionary::new("steps".into(), String::new(), DictionaryConstraints::default());
		// Smooth ramps of varying length: a few points reconstruct them exactly.
		for i in 0..(STEP_CALIBRATION_SAMPLES + 20) {
			let len = 10 + i % 7;
			#[allow(clippy::cast_precision_loss)]
			let amps: Vec<f64> = (0..len).map(|j| j as f64 / (len - 1) as f64).collect();
			dictionary.import_pattern(create_test_pattern(amps)).unwrap();
		}
		let steps = dictionary.learned_similarity().steps().expect("frozen after calibration");
		assert!((2..=4).contains(&steps), "a straight ramp needs few steps, got {steps}");
		assert!(dictionary.patterns().iter().all(|p| p.relatives().len() == steps), "every stored pattern uses the learned count");
		let total: usize = dictionary.patterns().iter().map(Pattern::occurrence_count).sum();
		assert!(total >= STEP_CALIBRATION_SAMPLES + 20, "no occurrence lost");
	}

	#[test]
	fn a_detailed_shape_keeps_enough_steps() {
		let wave: Vec<f64> = (0..40).map(|j| (f64::from(j) * 0.9).sin()).collect();
		let steps = choose_steps(&vec![wave.clone(); 8]).unwrap();
		assert!(steps > 20, "a fast wave needs many steps, got {steps}");
		assert!(resample_loss(&wave, steps) <= STEP_RESAMPLE_TOLERANCE);
	}

	#[test]
	#[allow(clippy::unnecessary_wraps)]
	fn restore_is_verbatim_and_skips_known_ids() -> Result<()> {
		let a = create_test_pattern(vec![1.0, 2.0]);
		let b = create_test_pattern(vec![1.0, 2.0]);
		let mut learned = Dictionary::new("l".into(), String::new(), DictionaryConstraints::new(None, None));
		learned.restore_patterns(vec![a.clone(), b.clone(), a]);
		assert_eq!(learned.len(), 2, "restore keeps both identical shapes (no merging) and drops the repeated id");
		assert_eq!(learned.patterns()[1], b);
		Ok(())
	}
}
