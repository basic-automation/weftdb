#[cfg(test)]
mod pattern_fix_test {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::Utc;
	use weftdb::{AspectId, DatabaseInfo};

	use crate::types::{MeasurementVector, Occurrence, Pattern, PatternID, Relative};

	fn create_test_pattern(amplitudes: &[f64]) -> Pattern {
		let pattern_id = PatternID::new();
		let database_info = DatabaseInfo::new("TestDB".to_string(), "/tmp/test".to_string());
		let occurrence = Occurrence::new(AspectId::new(), splimes::Resolution::Seconds, amplitudes.len(), database_info, pattern_id, Utc::now(), Utc::now());

		let relatives: Vec<Relative> = amplitudes
			.iter()
			.enumerate()
			.map(|(i, &amplitude)| {
				let location = BigDecimal::from_f64(i as f64).unwrap();
				let amplitude = BigDecimal::from_f64(amplitude).unwrap();
				let vector = MeasurementVector::new(location, amplitude);
				let max_x = BigDecimal::from_f64((amplitudes.len() - 1) as f64).unwrap();
				let max_y = BigDecimal::from_f64(1.0).unwrap();
				Relative::new(vector, max_x, max_y)
			})
			.collect();

		Pattern::new(pattern_id, vec![occurrence], relatives)
	}

	#[tokio::test]
	async fn test_pattern_comparison_fix() {
		use splimes::Spline;

		use crate::types::{Dictionary, DictionaryConstraints, Variability, VariablilityType};

		// Create dictionary with extremely low variability threshold
		let constraints = DictionaryConstraints::new(
			Some(crate::types::Steps::new(4, Spline::Linear)),
			Some(vec![VariablilityType::AveragePercentile(Variability::new(
				BigDecimal::from_f64(0.000_000_001).unwrap(), // Extremely low threshold
			))]),
		);

		let mut dictionary = Dictionary::new("Fix Test Dictionary".to_string(), "Test dictionary to verify the pattern comparison fix".to_string(), constraints);

		// Create pattern with peak at position 0: [1, 0, 0, 0]
		let pattern1 = create_test_pattern(&[1.0, 0.0, 0.0, 0.0]);

		// Create pattern with peak at position 1: [0, 1, 0, 0]
		let pattern2 = create_test_pattern(&[0.0, 1.0, 0.0, 0.0]);
		println!("Testing pattern comparison fix...");
		println!("Pattern 1: [1, 0, 0, 0] (peak at position 0)");
		println!("Pattern 2: [0, 1, 0, 0] (peak at position 1)");
		println!("Threshold: 0.000000001%");

		// Import first pattern
		dictionary.import_pattern(pattern1).unwrap();
		println!("After importing pattern 1: {} patterns", dictionary.len());

		// Import second pattern - should NOT merge due to fix
		dictionary.import_pattern(pattern2).unwrap();
		println!("After importing pattern 2: {} patterns", dictionary.len());

		// With the fix, we should have 2 separate patterns (not merged)
		assert_eq!(dictionary.len(), 2, "Patterns should NOT merge with the fix applied");

		println!("SUCCESS: Patterns correctly recognized as different!");
	}
}
