use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use splimes::{auto_interpolate, Point, Resolution as SplimesResolution, Spline};
use uuid::Uuid;

use crate::types::{pattern::Pattern, MeasurementVector, Relative};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Dictionary {
	pub id: Uuid,
	pub name: String,
	pub description: String,
	pub patterns: Vec<Pattern>,
	pub constraints: DictionaryConstraints,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DictionaryConstraints {
	pub steps: Option<Steps>,
	pub variabilities: Option<Vec<VariablilityType>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Steps {
	pub count: usize,
	pub interpolation: Spline,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Variability {
	pub value: BigDecimal,
}

impl Dictionary {
	/// Create a new dictionary with the given constraints
	pub fn new(name: String, description: String, constraints: DictionaryConstraints) -> Self {
		let id = Uuid::new_v4();
		Self { id, name, description, patterns: Vec::new(), constraints }
	}

	/// Import a new pattern into the dictionary
	///
	/// This function enforces the dictionary constraints and checks for pattern similarity.
	/// If the pattern matches one or more existing patterns based on the variability constraints,
	/// the new occurrence will be added to all matching patterns.
	/// If no matches are found, a new pattern will be created.
	pub async fn import_pattern(&mut self, mut pattern: Pattern) -> Result<()> {
		// First, enforce steps constraint if it exists
		if let Some(ref steps_config) = self.constraints.steps {
			pattern = self.convert_pattern_steps(pattern, steps_config).await?;
		}

		// Check for similarity with all existing patterns
		let mut matching_pattern_indices = Vec::new();
		for (index, existing_pattern) in self.patterns.iter().enumerate() {
			if self.patterns_are_similar(existing_pattern, &pattern)? {
				matching_pattern_indices.push(index);
			}
		}

		if !matching_pattern_indices.is_empty() {
			// Merge occurrences with all matching patterns
			for &index in &matching_pattern_indices {
				self.merge_pattern_occurrences_at_index(index, pattern.clone())?;
			}
		} else {
			// No similar pattern found, add as new pattern
			self.patterns.push(pattern);
		}

		Ok(())
	}

	/// Convert pattern to match the required number of steps using auto_interpolate
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

		// Convert relatives to points for interpolation
		let mut points: Vec<Point> = relatives
			.iter()
			.enumerate()
			.map(|(i, relative)| {
				let timestamp = Utc::now() + chrono::Duration::seconds(i as i64);
				Point::new(timestamp, relative.vector().amplitude().clone())
			})
			.collect();

		// If we only have one point, we can't interpolate - just duplicate it
		if points.len() == 1 {
			let single_amplitude = &points[0].value;
			let new_relatives: Vec<Relative> = (0..required_steps)
				.map(|i| {
					let location = BigDecimal::from_f64(i as f64 / (required_steps - 1) as f64).unwrap_or_else(|| BigDecimal::from(0));
					let vector = MeasurementVector::new(location, single_amplitude.clone());
					// Use the same max_x and max_y from the original relative
					Relative::new(vector, relatives[0].max_x().clone(), relatives[0].max_y().clone())
				})
				.collect();

			let new_pattern = Pattern::new(pattern.id(), pattern.occurrences().clone(), new_relatives);
			return Ok(new_pattern);
		}

		// Use auto_interpolate from splimes for sophisticated interpolation
		let start_time = points[0].timestamp;
		let end_time = points[points.len() - 1].timestamp;
		let resolution = SplimesResolution::Seconds;

		// Calculate the time duration and step size for required steps
		let total_duration = end_time.signed_duration_since(start_time);
		let step_duration = total_duration / (required_steps - 1) as i32;

		// Ensure we have enough time range for interpolation
		let interpolation_end_time = start_time + step_duration * (required_steps - 1) as i32;

		// Use auto_interpolate with the specified spline type from steps_config
		let interpolated_points = auto_interpolate(&mut points, start_time, interpolation_end_time, resolution, steps_config.interpolation).await?;

		// Convert interpolated points back to relatives
		let new_relatives: Vec<Relative> = interpolated_points
			.into_iter()
			.enumerate()
			.map(|(i, point)| {
				let location = BigDecimal::from_f64(i as f64 / (required_steps - 1) as f64).unwrap_or_else(|| BigDecimal::from(0));
				let vector = MeasurementVector::new(location, point.value);

				// Use average max_x and max_y from original relatives
				let avg_max_x = self.calculate_average_max_x(relatives);
				let avg_max_y = self.calculate_average_max_y(relatives);
				Relative::new(vector, avg_max_x, avg_max_y)
			})
			.collect();

		let new_pattern = Pattern::new(pattern.id(), pattern.occurrences().clone(), new_relatives);
		Ok(new_pattern)
	}

	/// Check if two patterns are similar based on the dictionary's variability constraints
	fn patterns_are_similar(&self, pattern1: &Pattern, pattern2: &Pattern) -> Result<bool> {
		if pattern1.amplitudes().len() != pattern2.amplitudes().len() {
			return Ok(false);
		}

		if let Some(variabilities) = &self.constraints.variabilities {
			let mut sorted_vari = variabilities.clone();
			sorted_vari.sort_by_key(|v| match v {
				VariablilityType::SumStatic(_) | VariablilityType::SumPercentile(_) | VariablilityType::AbsoluteSumStatic(_) | VariablilityType::AbsoluteSumPercentile(_) => 0, // Cheapest: single value ops

				VariablilityType::AverageStatic(_) | VariablilityType::AveragePercentile(_) | VariablilityType::AbsoluteAverageStatic(_) | VariablilityType::AbsoluteAveragePercentile(_) => 1, // Average: sum then divide

				VariablilityType::MaximumStatic(_) | VariablilityType::MaximumPercentile(_) | VariablilityType::AbsoluteMaximumStatic(_) | VariablilityType::AbsoluteMaximumPercentile(_) => 2, // Max: iterate once

				_ => 3,
			});

			for variability in sorted_vari {
				if !self.check_variability_constraint(pattern1, pattern2, &variability)? {
					return Ok(false);
				}
			}
			Ok(true)
		} else {
			Ok(false)
		}
	}

	/// Check a specific variability constraint between two patterns
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

	/// Check absolute sum static variability
	fn check_absolute_sum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let sum1 = pattern1.abs_sum();
		let sum2 = pattern2.abs_sum();
		Ok((sum1 - sum2).abs() <= *threshold)
	}

	/// Check absolute sum percentile variability
	fn check_absolute_sum_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let sum1 = pattern1.abs_sum();
		let sum2 = pattern2.abs_sum();
		let diff = (sum1 - sum2).abs();
		let percentile = if sum2.is_zero() { BigDecimal::zero() } else { (&diff / sum2) * BigDecimal::from(100) };
		Ok(percentile <= *threshold)
	}

	/// Check sum static variability
	fn check_sum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let sum1 = pattern1.sum();
		let sum2 = pattern2.sum();
		Ok((sum1 - sum2).abs() <= *threshold)
	}

	/// Check sum percentile variability
	fn check_sum_percentile(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let sum1 = pattern1.sum();
		let sum2 = pattern2.sum();
		let diff = (sum1 - sum2).abs();
		let percentile = if sum2.is_zero() { BigDecimal::zero() } else { (&diff / sum2) * BigDecimal::from(100) };
		Ok(percentile <= *threshold)
	}

	/// Check maximum static variability
	fn check_maximum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let max_diff = pattern1.amplitudes().iter().zip(pattern2.amplitudes()).map(|(a1, a2)| (a1 - a2).abs()).max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or_else(BigDecimal::zero);
		Ok(max_diff <= *threshold)
	}

	/// Check average static variability
	fn check_average_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_diff = pattern1.amplitudes().iter().zip(pattern2.amplitudes()).map(|(a1, a2)| (a1 - a2).abs()).fold(BigDecimal::zero(), |acc, d| acc + d);
		let avg_diff = sum_diff / len;
		Ok(avg_diff <= *threshold)
	}

	/// Check absolute maximum static variability
	fn check_absolute_maximum_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let max_diff = pattern1.amplitudes().iter().zip(pattern2.amplitudes()).map(|(a1, a2)| (a1.abs() - a2.abs()).abs()).max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or_else(BigDecimal::zero);
		Ok(max_diff <= *threshold)
	}

	/// Check absolute average static variability
	fn check_absolute_average_static(&self, pattern1: &Pattern, pattern2: &Pattern, threshold: &BigDecimal) -> Result<bool> {
		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_diff = pattern1.amplitudes().iter().zip(pattern2.amplitudes()).map(|(a1, a2)| (a1.abs() - a2.abs()).abs()).fold(BigDecimal::zero(), |acc, d| acc + d);
		let avg_diff = sum_diff / len;
		Ok(avg_diff <= *threshold)
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
		let len = BigDecimal::from_usize(pattern1.amplitudes().len()).unwrap();
		let sum_percent = pattern1
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
			.fold(BigDecimal::zero(), |acc, p| acc + p);
		let avg_percent = sum_percent / len;
		Ok(avg_percent <= *threshold)
	}

	/// Merge occurrences from one pattern into another at a specific index
	fn merge_pattern_occurrences_at_index(&mut self, target_index: usize, source_pattern: Pattern) -> Result<()> {
		if let Some(target_pattern) = self.patterns.get_mut(target_index) {
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
}

#[cfg(test)]
mod tests {
	use bigdecimal::FromPrimitive;
	use chrono::Utc;
	use database::{AspectId, DatabaseInfo};
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
                                        value: BigDecimal::from_f64(0.1).unwrap(), // 10% threshold
                                })]),
		};

		Dictionary::new("Test Dictionary".to_string(), "A test dictionary for pattern matching".to_string(), constraints)
	}

	#[tokio::test]
	async fn test_import_new_pattern() {
		let mut dictionary = create_test_dictionary();
		let pattern = create_test_pattern(vec![1.0, 2.0, 3.0, 2.0, 1.0]);

		let result = dictionary.import_pattern(pattern).await;
		assert!(result.is_ok());
		assert_eq!(dictionary.patterns.len(), 1);
	}

	#[tokio::test]
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
	async fn test_import_pattern_matches_multiple_distinct_patterns() {
		// Create a dictionary with moderately loose constraints to allow some matches
		let constraints = DictionaryConstraints {
			steps: Some(Steps { count: 10, interpolation: Spline::Linear }),
			variabilities: Some(vec![VariablilityType::AbsoluteSumPercentile(Variability {
				value: BigDecimal::from_f64(0.3).unwrap(), // 30% threshold
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
	async fn test_import_pattern_matches_multiple_existing_patterns() {
		// Create a dictionary with very loose constraints to enable multiple matches
		let constraints = DictionaryConstraints {
			steps: Some(Steps { count: 5, interpolation: Spline::Linear }),
			variabilities: Some(vec![VariablilityType::AbsoluteSumPercentile(Variability {
				value: BigDecimal::from_f64(0.6).unwrap(), // 60% threshold - very loose
			})]),
		};
		let mut dictionary = Dictionary::new("Very Loose Test Dictionary".to_string(), "A test dictionary with very loose constraints for multiple matches".to_string(), constraints);

		// Import first pattern - sum = 5.0
		let pattern1 = create_test_pattern(vec![1.0, 1.0, 1.0, 1.0, 1.0]);
		dictionary.import_pattern(pattern1).await.unwrap();

		// Import second pattern - sum = 10.0 (different enough to remain separate)
		// Difference: |10.0 - 5.0|/10.0 = 50% < 60%, so these will merge
		// Let me use a larger difference
		let pattern2 = create_test_pattern(vec![3.0, 3.0, 3.0, 3.0, 3.0]); // sum = 15.0
		dictionary.import_pattern(pattern2).await.unwrap();
		// Difference: |15.0 - 5.0|/15.0 = 66.7% > 60%, so these should remain separate

		// Verify they are separate
		assert_eq!(dictionary.patterns.len(), 2);

		// Now import a pattern that matches both within the 60% threshold
		// We need: |sum - 5.0|/max(sum, 5.0) <= 0.6 AND |sum - 15.0|/max(sum, 15.0) <= 0.6
		// For sum = 8.0: |8.0 - 5.0|/8.0 = 37.5% < 60% ✓
		// For sum = 8.0: |15.0 - 8.0|/15.0 = 46.7% < 60% ✓
		let pattern3 = create_test_pattern(vec![1.6, 1.6, 1.6, 1.6, 1.6]); // sum = 8.0
		dictionary.import_pattern(pattern3).await.unwrap();

		// Should still have 2 patterns, but both should now have 2 occurrences each
		// (the new pattern should match both existing patterns)
		assert_eq!(dictionary.patterns.len(), 2);
		assert_eq!(dictionary.patterns[0].occurrences().len(), 2); // pattern1 + pattern3
		assert_eq!(dictionary.patterns[1].occurrences().len(), 2); // pattern2 + pattern3
	}
}
