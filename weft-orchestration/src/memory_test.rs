#[cfg(test)]
mod memory_tests {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::Utc;
	use serial_test::serial;
	use splimes::{Resolution, Spline};
	use weftdb::{AspectId, DatabaseInfo};

	use crate::types::{Dictionary, DictionaryConstraints, MeasurementVector, Occurrence, Pattern, PatternID, Relative, Steps, Variability, VariablilityType};

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

	fn create_test_dict() -> Dictionary {
		let constraints = DictionaryConstraints::new(
			Some(Steps::new(10, Spline::Linear)),
			Some(vec![VariablilityType::AbsoluteSumPercentile(Variability::new(
				BigDecimal::from_f64(15.0).unwrap(), // 15% threshold for memory tests - more lenient than the 10% in regular tests
			))]),
		);
		Dictionary::new("test_dict".to_string(), "Test dictionary".to_string(), constraints)
	}

	// Generate varying test pattern amplitudes
	fn generate_amplitudes(id: usize, length: usize) -> Vec<f64> {
		(0..length).map(|i| (i as f64 * (id as f64).mul_add(0.01, 1.0)).sin() * (id as f64).mul_add(0.1, 1.0)).collect()
	}

	#[tokio::test]
	#[serial] // Run serially to prevent memory contention
	async fn test_memory_usage_small() {
		println!("Testing memory usage with 100 patterns...");
		let mut dict = create_test_dict();

		for i in 0..100 {
			let amplitudes = generate_amplitudes(i, 32);
			let pattern = create_test_pattern(amplitudes);
			dict.import_pattern(pattern).expect("Failed to import pattern");
		}

		println!("Successfully imported 100 patterns");
		println!("Dictionary patterns count: {}", dict.len());
	}

	#[tokio::test]
	#[serial] // Run serially to prevent memory contention
	async fn test_memory_usage_medium() {
		println!("Testing memory usage with 500 patterns...");
		let mut dict = create_test_dict();

		for i in 0..500 {
			let amplitudes = generate_amplitudes(i, 32);
			let pattern = create_test_pattern(amplitudes);
			dict.import_pattern(pattern).expect("Failed to import pattern");
		}

		println!("Successfully imported 500 patterns");
		println!("Dictionary patterns count: {}", dict.len());
	}

	#[tokio::test]
	#[serial] // Run serially to prevent memory contention
	async fn test_memory_usage_large() {
		println!("Testing memory usage with 1000 patterns...");
		let mut dict = create_test_dict();

		for i in 0..1000 {
			if i % 100 == 0 {
				println!("Imported {i} patterns");
			}
			let amplitudes = generate_amplitudes(i, 32);
			let pattern = create_test_pattern(amplitudes);
			dict.import_pattern(pattern).expect("Failed to import pattern");
		}

		println!("Successfully imported 1000 patterns");
		println!("Dictionary patterns count: {}", dict.len());
	}
}
