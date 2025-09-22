use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use splimes::Spline;
use uuid::Uuid;
use wide::f64x4;

use crate::types::{pattern::Pattern, MeasurementVector, Relative};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dictionary {
	pub id: Uuid,
	pub name: String,
	pub description: String,
	pub patterns: Vec<Pattern>,
	pub constraints: DictionaryConstraints,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DictionaryConstraints {
	pub steps: Option<Steps>,
	pub variabilities: Option<Vec<VariablilityType>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Steps {
	pub count: usize,
	pub interpolation: Spline,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variability {
	pub value: BigDecimal,
}

impl Dictionary {
	/// Create a new dictionary with the given constraints
	pub fn new(name: String, description: String, constraints: DictionaryConstraints) -> Self {
		let id = Uuid::new_v4();
		Self { id, name, description, patterns: Vec::new(), constraints }
	}

	/// Import a new pattern into the dictionary following the exact specification
	///
	/// ALL patterns are processed identically regardless of dictionary size:
	/// 1. Enforce steps constraint if configured
	/// 2. Check similarity against ALL existing patterns using configured variability constraints
	/// 3. If similar patterns found, merge occurrences; otherwise add as new pattern
	pub async fn import_pattern(&mut self, mut new_pattern: Pattern) -> Result<()> {
		// Step 1: Enforce steps constraint if configured
		if let Some(steps_config) = &self.constraints.steps {
			new_pattern = self.convert_pattern_steps(new_pattern, steps_config).await?;
		}

		// Step 2: Check similarity against ALL existing patterns using configured variability constraints
		// This is the specification-compliant approach - no shortcuts or optimizations that change behavior
		let mut matching_indices = Vec::new();

		for (idx, existing_pattern) in self.patterns.iter().enumerate() {
			if self.patterns_are_similar(&new_pattern, existing_pattern)? {
				matching_indices.push(idx);
			}
		}

		// Step 3: Handle matches according to specification
		if !matching_indices.is_empty() {
			// "If they are deemed sufficiently similar the new pattern will be converted to an
			// occurrence of the existing pattern" - merge with ALL matching patterns as specified
			for &idx in &matching_indices {
				self.merge_pattern_occurrences_at_index(idx, new_pattern.clone())?;
			}
		} else {
			// "If the new pattern is not found to be efficiently similar to any existing pattern
			// then the new pattern will be added to the pattern dictionary"
			self.patterns.push(new_pattern);
		}

		Ok(())
	}

	/// Convert pattern to match the required number of steps using direct interpolation
	async fn convert_pattern_steps(&self, pattern: Pattern, steps_config: &Steps) -> Result<Pattern> {
		let relatives = pattern.relatives();
		if relatives.is_empty() {
			return Ok(pattern);
		}

		let current_steps = relatives.len();
		let required_steps = steps_config.count;

		if current_steps == required_steps {
			return Ok(pattern);
		}

		// If we only have one point, duplicate it across all required steps
		if relatives.len() == 1 {
			let single_relative = &relatives[0];
			let new_relatives: Vec<Relative> = (0..required_steps)
				.map(|i| {
					let location = BigDecimal::from_f64(i as f64 / (required_steps - 1) as f64).unwrap_or_else(|| BigDecimal::from(0));
					let vector = MeasurementVector::new(location, single_relative.vector().amplitude().clone());
					Relative::new(vector, single_relative.max_x().clone(), single_relative.max_y().clone())
				})
				.collect();

			let new_pattern = Pattern::new(pattern.id(), pattern.occurrences().clone(), new_relatives);
			return Ok(new_pattern);
		}

		println!("DEBUG: convert_pattern_steps - current_steps: {}, required_steps: {}", current_steps, required_steps);

		// Perform direct interpolation on the relative data
		let new_relatives: Vec<Relative> = (0..required_steps)
			.map(|i| {
				// Calculate target location (0.0 to 1.0)
				let target_location = if required_steps == 1 { BigDecimal::from(0) } else { BigDecimal::from_f64(i as f64 / (required_steps - 1) as f64).unwrap_or_else(|| BigDecimal::from(0)) };

				// Find the interpolated amplitude at this location using linear interpolation
				let interpolated_amplitude = interpolate_amplitude_at_location(relatives, &target_location);

				// Use average max_x and max_y from original relatives
				let avg_max_x = self.calculate_average_max_x(relatives);
				let avg_max_y = self.calculate_average_max_y(relatives);

				let vector = MeasurementVector::new(target_location, interpolated_amplitude);
				Relative::new(vector, avg_max_x, avg_max_y)
			})
			.collect();

		println!("DEBUG: input relatives count: {}, output relatives count: {}", relatives.len(), new_relatives.len());

		let new_pattern = Pattern::new(pattern.id(), pattern.occurrences().clone(), new_relatives);
		Ok(new_pattern)
	}

	/// Check if two patterns are similar based on the dictionary's variability constraints
	pub fn patterns_are_similar(&self, pattern1: &Pattern, pattern2: &Pattern) -> Result<bool> {
		if pattern1.amplitudes().len() != pattern2.amplitudes().len() {
			return Ok(false);
		}

		if let Some(variabilities) = &self.constraints.variabilities {
			// All variability constraints must be satisfied for patterns to be considered similar
			for variability in variabilities {
				if !self.check_variability_constraint(pattern1, pattern2, variability)? {
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
	fn check_variability_constraint(&self, pattern1: &Pattern, pattern2: &Pattern, variability: &VariablilityType) -> Result<bool> {
		match variability {
			VariablilityType::MaximumStatic(static_var) => self.check_maximum_static(pattern1, pattern2, &static_var.value),
			VariablilityType::AverageStatic(static_var) => self.check_average_static(pattern1, pattern2, &static_var.value),
			VariablilityType::AbsoluteMaximumStatic(static_var) => self.check_absolute_maximum_static(pattern1, pattern2, &static_var.value),
			VariablilityType::AbsoluteAverageStatic(static_var) => self.check_absolute_average_static(pattern1, pattern2, &static_var.value),
			VariablilityType::MaximumPercentile(percentile_var) => self.check_maximum_percentile(pattern1, pattern2, &percentile_var.value),
			VariablilityType::AveragePercentile(percentile_var) => self.check_average_percentile(pattern1, pattern2, &percentile_var.value),
			VariablilityType::AbsoluteMaximumPercentile(percentile_var) => self.check_absolute_maximum_percentile(pattern1, pattern2, &percentile_var.value),
			VariablilityType::AbsoluteAveragePercentile(percentile_var) => self.check_absolute_average_percentile(pattern1, pattern2, &percentile_var.value),
			VariablilityType::SumStatic(static_var) => self.check_sum_static(pattern1, pattern2, &static_var.value),
			VariablilityType::SumPercentile(static_var) => self.check_sum_percentile(pattern1, pattern2, &static_var.value),
			VariablilityType::AbsoluteSumStatic(static_var) => self.check_absolute_sum_static(pattern1, pattern2, &static_var.value),
			VariablilityType::AbsoluteSumPercentile(static_var) => self.check_absolute_sum_percentile(pattern1, pattern2, &static_var.value),
		}
	}

	/// Check absolute sum percentile variability
	fn check_absolute_sum_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		// Handle empty patterns
		if pattern1.amplitudes().is_empty() || pattern2.amplitudes().is_empty() {
			return Ok(true); // Empty patterns are considered similar
		}

		let sum1 = pattern1.abs_sum();
		let sum2 = pattern2.abs_sum();
		let diff = (sum1 - sum2).abs();
		let percentile = if sum2.is_zero() { BigDecimal::zero() } else { (&diff / sum2) * BigDecimal::from(100) };
		Ok(percentile <= *threshold)
	}

	/// Check sum percentile variability
	fn check_sum_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		// Handle empty patterns
		if pattern1.amplitudes().is_empty() || pattern2.amplitudes().is_empty() {
			return Ok(true); // Empty patterns are considered similar
		}

		let sum1 = pattern1.sum();
		let sum2 = pattern2.sum();
		let diff = (sum1 - sum2).abs();
		let percentile = if sum2.is_zero() { BigDecimal::zero() } else { (&diff / sum2) * BigDecimal::from(100) };
		Ok(percentile <= *threshold)
	}

	/// Check maximum percentile variability
	fn check_maximum_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let max_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1 - a2).abs();
				if a2.is_zero() {
					BigDecimal::zero()
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
			.unwrap_or_else(BigDecimal::zero);
		Ok(max_percent <= *threshold)
	}

	/// Check average percentile variability
	fn check_average_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1 - a2).abs();
				if a2.is_zero() {
					BigDecimal::zero()
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.fold(BigDecimal::zero(), |acc, p| acc + p);
		let avg_percent = sum_percent / len;
		Ok(avg_percent <= *threshold)
	}

	/// Check absolute maximum percentile variability
	fn check_absolute_maximum_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let max_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1.abs() - a2.abs()).abs();
				if a2.is_zero() {
					BigDecimal::zero()
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
			.unwrap_or_else(BigDecimal::zero);
		Ok(max_percent <= *threshold)
	}

	/// Check absolute average percentile variability
	fn check_absolute_average_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		// Handle empty patterns
		if pattern1.amplitudes().is_empty() || pattern2.amplitudes().is_empty() {
			return Ok(true); // Empty patterns are considered similar
		}

		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_percent = pattern1
			.amplitudes()
			.iter()
			.zip(pattern2.amplitudes())
			.map(|(a1, a2)| {
				let diff = (a1.abs() - a2.abs()).abs();
				if a2.abs().is_zero() {
					BigDecimal::zero()
				} else {
					(diff / a2.abs()) * BigDecimal::from(100)
				}
			})
			.fold(BigDecimal::zero(), |acc, p| acc + p);
		let avg_percent = sum_percent / len;

		Ok(avg_percent <= *threshold)
	}

	/// Merge occurrences from one pattern into another at a specific index
	pub fn merge_pattern_occurrences_at_index(&mut self, target_index: usize, source_pattern: Pattern) -> Result<()> {
		if let Some(target_pattern) = self.patterns.get_mut(target_index) {
			// Add all occurrences from source pattern to target pattern
			for occurrence in source_pattern.occurrences() {
				target_pattern.add_occurrence(occurrence.clone());
			}
		}
		Ok(())
	}

	/// Calculate average max_x from relatives
	fn calculate_average_max_x(&self, relatives: &[Relative]) -> BigDecimal {
		if relatives.is_empty() {
			return BigDecimal::from(1);
		}

		let sum = relatives.iter().fold(BigDecimal::from(0), |acc, rel| acc + rel.max_x());
		let len = BigDecimal::from(relatives.len() as i64);
		sum / len
	}

	/// Calculate average max_y from relatives
	fn calculate_average_max_y(&self, relatives: &[Relative]) -> BigDecimal {
		if relatives.is_empty() {
			return BigDecimal::from(1);
		}

		let sum = relatives.iter().fold(BigDecimal::from(0), |acc, rel| acc + rel.max_y());
		let len = BigDecimal::from(relatives.len() as i64);
		sum / len
	}

	pub fn len(&self) -> usize {
		self.patterns.len()
	}

	pub fn is_empty(&self) -> bool {
		self.patterns.is_empty()
	}

	// SIMD-optimized constraint checking methods for better performance

	/// SIMD-optimized maximum static constraint checking
	fn check_maximum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(|a| a.to_f64()).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(|a| a.to_f64()).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return Ok(false);
		}

		let max1 = amps1.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
		let max2 = amps2.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		Ok((max1 - max2).abs() <= threshold_f64)
	}

	/// SIMD-optimized average static constraint checking  
	fn check_average_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(|a| a.to_f64()).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(|a| a.to_f64()).collect();

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

		let avg_diff = sum_diff / amps1.len() as f64;
		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		Ok(avg_diff <= threshold_f64)
	}

	/// SIMD-optimized absolute maximum static constraint checking
	fn check_absolute_maximum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(|a| a.to_f64()).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(|a| a.to_f64()).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return Ok(false);
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
		Ok((abs_max1 - abs_max2).abs() <= threshold_f64)
	}

	/// SIMD-optimized absolute average static constraint checking
	fn check_absolute_average_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(|a| a.to_f64()).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(|a| a.to_f64()).collect();

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

		let abs_avg1 = abs_sum1 / amps1.len() as f64;
		let abs_avg2 = abs_sum2 / amps2.len() as f64;

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		Ok((abs_avg1 - abs_avg2).abs() <= threshold_f64)
	}

	/// SIMD-optimized absolute sum static constraint checking
	fn check_absolute_sum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(|a| a.to_f64()).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(|a| a.to_f64()).collect();

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

		let threshold_f64 = threshold.to_f64().unwrap_or(0.0);
		Ok((abs_sum1 - abs_sum2).abs() <= threshold_f64)
	}

	/// SIMD-optimized sum static constraint checking
	fn check_sum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let amps1: Vec<f64> = pattern1.amplitudes().iter().filter_map(|a| a.to_f64()).collect();
		let amps2: Vec<f64> = pattern2.amplitudes().iter().filter_map(|a| a.to_f64()).collect();

		if amps1.is_empty() || amps2.is_empty() || amps1.len() != amps2.len() {
			return Ok(false);
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
		Ok((sum1 - sum2).abs() <= threshold_f64)
	}

	/// Merge another dictionary into this one
	/// All patterns from the other dictionary are imported using the standard import_pattern logic
	/// This ensures all similarity constraints are respected during the merge
	pub async fn merge_dictionary(&mut self, other: Dictionary) -> Result<()> {
		// Import each pattern from the other dictionary
		for pattern in other.patterns {
			self.import_pattern(pattern).await?;
		}
		Ok(())
	}

	/// Serialize the dictionary to JSON string
	pub fn to_json(&self) -> Result<String> {
		serde_json::to_string(self).map_err(|e| anyhow::anyhow!("Failed to serialize dictionary: {}", e))
	}

	/// Serialize the dictionary to JSON string with pretty formatting
	pub fn to_json_pretty(&self) -> Result<String> {
		serde_json::to_string_pretty(self).map_err(|e| anyhow::anyhow!("Failed to serialize dictionary: {}", e))
	}

	/// Deserialize a dictionary from JSON string
	pub fn from_json(json: &str) -> Result<Self> {
		serde_json::from_str(json).map_err(|e| anyhow::anyhow!("Failed to deserialize dictionary: {}", e))
	}

	/// Serialize the dictionary to binary format (using bincode)
	pub fn to_bytes(&self) -> Result<Vec<u8>> {
		bincode::serialize(self).map_err(|e| anyhow::anyhow!("Failed to serialize dictionary to bytes: {}", e))
	}

	/// Deserialize a dictionary from binary format (using bincode)
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		bincode::deserialize(bytes).map_err(|e| anyhow::anyhow!("Failed to deserialize dictionary from bytes: {}", e))
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
	let interpolated_amplitude = left_amp + (right_amp - left_amp) * interpolation_factor;

	BigDecimal::from_f64(interpolated_amplitude).unwrap_or_else(BigDecimal::zero)
}

#[cfg(test)]
mod tests {
	use bigdecimal::FromPrimitive;
	use chrono::Utc;
	use database::{AspectId, DatabaseInfo};
	use serial_test::serial;
	use splimes::Resolution;

	use super::*;
	use crate::types::{
		pattern::{Occurrence, PatternID}, MeasurementVector, Relative
	};

	fn create_test_pattern(amplitudes: Vec<f64>) -> Pattern {
		let pattern_id = PatternID::new();
		let occurrences = vec![Occurrence::new(AspectId::new(), Resolution::Seconds, amplitudes.len(), DatabaseInfo::new("test".to_string(), "test_path".to_string()), pattern_id, Utc::now(), Utc::now())];

		let relatives: Vec<Relative> = amplitudes
			.into_iter()
			.enumerate()
			.map(|(i, amp)| {
				let location = BigDecimal::from_f64(i as f64 / 10.0).unwrap();
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

	#[tokio::test]
	#[serial]
	async fn test_import_new_pattern() {
		let mut dictionary = create_test_dictionary();
		let pattern = create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]);

		let result = dictionary.import_pattern(pattern).await;
		assert!(result.is_ok());
		assert_eq!(dictionary.patterns.len(), 1);
	}

	#[tokio::test]
	#[serial]
	async fn test_import_similar_pattern() {
		let mut dictionary = create_test_dictionary();

		// Import first pattern
		let pattern1 = create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]);
		dictionary.import_pattern(pattern1).await.unwrap();

		// Import similar pattern (should merge with existing)
		let pattern2 = create_test_pattern(vec![1.1, 2.1, 3.1, 2.1, 1.1]);
		dictionary.import_pattern(pattern2).await.unwrap();

		// Should still have only one pattern, but with two occurrences
		assert_eq!(dictionary.patterns.len(), 1);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2);
	}

	#[tokio::test]
	#[serial]
	async fn test_import_different_pattern() {
		let mut dictionary = create_test_dictionary();

		// Import first pattern
		let pattern1 = create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]);
		dictionary.import_pattern(pattern1).await.unwrap();

		// Import very different pattern
		let pattern2 = create_test_pattern(vec![10.0, 20.0, 30.0, 20.0, 10.0]);
		dictionary.import_pattern(pattern2).await.unwrap();

		// Should have two different patterns
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 1);
		assert_eq!(dictionary.patterns[1].occurrences().len(), 1);
	}

	#[tokio::test]
	#[serial]
	async fn test_import_pattern_matches_multiple() {
		let mut dictionary = create_test_dictionary();

		// Import first pattern
		let pattern1 = create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]);
		dictionary.import_pattern(pattern1).await.unwrap();

		// Import second similar pattern
		let pattern2 = create_test_pattern(vec![1.05, 2.05, 3.05, 2.05, 1.05]);
		dictionary.import_pattern(pattern2).await.unwrap();

		// Both should be merged into one pattern due to similarity
		assert_eq!(dictionary.patterns.len(), 1);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2);

		// Now import a pattern that could match both existing similar patterns
		// This pattern should be added to the existing pattern (which already contains both similar occurrences)
		let pattern3 = create_test_pattern(vec![1.08, 2.08, 3.08, 2.08, 1.08]);
		dictionary.import_pattern(pattern3).await.unwrap();

		// Should still have one pattern, now with three occurrences
		assert_eq!(dictionary.patterns.len(), 1);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 3);
	}

	#[tokio::test]
	#[serial]
	async fn test_import_pattern_matches_multiple_distinct_patterns() {
		// Create a dictionary with moderately loose constraints to allow some matches
		let constraints = DictionaryConstraints {
			steps: Some(Steps { count: 10, interpolation: Spline::Linear }),
			variabilities: Some(vec![VariablilityType::AbsoluteSumPercentile(Variability {
				value: BigDecimal::from_f64(30.0).unwrap(), // 30% threshold
			})]),
		};
		let mut dictionary = Dictionary::new("Moderate Test Dictionary".to_string(), "A test dictionary with moderate constraints".to_string(), constraints);

		// Import first pattern - sum = 9.0
		let pattern1 = create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]);
		dictionary.import_pattern(pattern1).await.unwrap();

		// Import a significantly different pattern - sum = 25.0
		let pattern2 = create_test_pattern(vec![3.0, 6.0, 10.0, 6.0, 0.0]);
		dictionary.import_pattern(pattern2).await.unwrap();

		// These should be separate patterns due to significant difference
		// Difference: |25.0 - 9.0| = 16.0, Percentage: 16.0/25.0 = 64% > 30% threshold
		assert_eq!(dictionary.patterns.len(), 2);

		// Now import a pattern that matches the first but not the second
		// Sum = 10.8 (within 30% of 9.0: |10.8 - 9.0|/10.8 = 16.7% < 30%)
		// But not within 30% of 25.0: |25.0 - 10.8|/25.0 = 56.8% > 30%
		let pattern3 = create_test_pattern(vec![1.2, 2.4, 3.6, 2.4, 1.2]);
		dictionary.import_pattern(pattern3).await.unwrap();

		// Should still have 2 patterns, first should have 2 occurrences, second should have 1
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2); // pattern1 + pattern3
		assert_eq!(dictionary.patterns[1].occurrences().len(), 1); // pattern2 only
	}

	#[tokio::test]
	#[serial]
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
		let pattern1 = create_test_pattern(vec![2.0, 2.0, 2.0]);
		dictionary.import_pattern(pattern1).await.unwrap();

		// Import second pattern - sum = 8.0 (should remain separate)
		// Difference: |8.0 - 6.0|/6.0 = 33.3% > 25%, so these remain separate
		let pattern2 = create_test_pattern(vec![3.0, 2.0, 3.0]);
		dictionary.import_pattern(pattern2).await.unwrap();

		// Verify they're separate
		assert_eq!(dictionary.patterns.len(), 2);

		// Now import a pattern that matches both
		// Sum = 7.0
		// vs pattern1 (sum 6.0): |7.0 - 6.0|/6.0 = 16.7% < 25% ✓
		// vs pattern2 (sum 8.0): |7.0 - 8.0|/8.0 = 12.5% < 25% ✓
		let pattern3 = create_test_pattern(vec![2.5, 2.0, 2.5]);
		dictionary.import_pattern(pattern3).await.unwrap();

		// Should still have 2 patterns, but BOTH should now have 2 occurrences
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2); // pattern1 + pattern3
		assert_eq!(dictionary.patterns[1].occurrences().len(), 2); // pattern2 + pattern3
	}
}
