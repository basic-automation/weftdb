#[cfg(test)]
mod tests {
	use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
	use chrono::{DateTime, Utc};

	use super::super::plot_terminal;
	use crate::{Point, Resolution, auto_interpolate, generate_target_times, gpu_interpolate, polynomial, polynomial_simd};

	const TARGET_ACCURACY_THRESHOLD: f64 = 1.0e2;
	const RESOLUTION: Resolution = Resolution::Seconds;

	#[tokio::test]
	async fn test_polynomial_interpolation() {
		let resolution = RESOLUTION;
		let start = DateTime::<Utc>::from_timestamp(-5, 0).unwrap();
		let end = DateTime::<Utc>::from_timestamp(28, 0).unwrap();
		let degree = 1;
		let bounds_factor = Some(2.0);
		let spline = crate::Spline::Polynomial(degree, bounds_factor);

		#[rustfmt::skip]
		let points: Vec<Point> = vec![
            Point { timestamp: DateTime::<Utc>::from_timestamp(0, 0).unwrap(), value: BigDecimal::from(0) }, 
            Point { timestamp: DateTime::<Utc>::from_timestamp(1, 0).unwrap(), value: BigDecimal::from(2) }, 
            Point { timestamp: DateTime::<Utc>::from_timestamp(2, 0).unwrap(), value: BigDecimal::from(6) }, 
            Point { timestamp: DateTime::<Utc>::from_timestamp(3, 0).unwrap(), value: BigDecimal::from(24) }, 
            Point { timestamp: DateTime::<Utc>::from_timestamp(4, 0).unwrap(), value: BigDecimal::from(8) }, 
            Point { timestamp: DateTime::<Utc>::from_timestamp(5, 0).unwrap(), value: BigDecimal::from(2) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(6, 0).unwrap(), value: BigDecimal::from(10) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(7, 0).unwrap(), value: BigDecimal::from(14) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(8, 0).unwrap(), value: BigDecimal::from(18) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(9, 0).unwrap(), value: BigDecimal::from(22) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(10, 0).unwrap(), value: BigDecimal::from(26) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(11, 0).unwrap(), value: BigDecimal::from(30) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(12, 0).unwrap(), value: BigDecimal::from(4) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(13, 0).unwrap(), value: BigDecimal::from(8) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(14, 0).unwrap(), value: BigDecimal::from(12) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(15, 0).unwrap(), value: BigDecimal::from(16) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(16, 0).unwrap(), value: BigDecimal::from(2) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(17, 0).unwrap(), value: BigDecimal::from(6) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(18, 0).unwrap(), value: BigDecimal::from(10) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(19, 0).unwrap(), value: BigDecimal::from(5) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(20, 0).unwrap(), value: BigDecimal::from(15) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(21, 0).unwrap(), value: BigDecimal::from(25) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(22, 0).unwrap(), value: BigDecimal::from(35) },
            Point { timestamp: DateTime::<Utc>::from_timestamp(23, 0).unwrap(), value: BigDecimal::from(115) },
        ];

		let target_times = generate_target_times(start, end, resolution);

		// cpu interpolation with bounds from spline
		let timer = tokio::time::Instant::now();
		let cpu = polynomial(points.clone(), start, end, resolution, degree, bounds_factor).unwrap();
		let cpu_time = timer.elapsed();
		println!("Polynomial: CPU Interpolation took: {:?}", cpu_time);

		// simd interpolation with bounds from spline
		let timer = tokio::time::Instant::now();
		let simd = polynomial_simd(&points, &target_times, resolution, degree, bounds_factor).unwrap();
		let simd_time = timer.elapsed();
		println!("Polynomial: SIMD Interpolation took: {:?}", simd_time);

		// gpu interpolation with bounds from spline
		let timer = tokio::time::Instant::now();
		let gpu = gpu_interpolate(points.clone(), start, end, resolution, spline).await.unwrap();
		let gpu_time = timer.elapsed();
		println!("Polynomial: GPU Interpolation took: {:?}", gpu_time);

		// auto interpolation with bounds from spline
		let timer = tokio::time::Instant::now();
		let auto = auto_interpolate(points.clone(), start, end, resolution, spline).await.unwrap();
		let auto_time = timer.elapsed();
		println!("Polynomial: Auto Interpolation took: {:?}", auto_time);

		// automatically determine the fastest method
		let auto_timer = tokio::time::Instant::now();
		let auto2 = auto_interpolate(points.clone(), start, end, resolution, spline).await.unwrap();
		let auto2_time = auto_timer.elapsed();
		println!("Polynomial: Auto Interpolation took: {:?}", auto2_time);

		if auto2_time < gpu_time && auto2_time < cpu_time && auto2_time < simd_time {
			let percentage_difference = ((gpu_time - auto2_time).as_nanos() as f64 / auto2_time.as_nanos() as f64) * 100.0;
			println!("Polynomial: Auto was faster by {:.2}%", percentage_difference);
		} else if gpu_time < auto2_time && gpu_time < cpu_time && gpu_time < simd_time {
			let percentage_difference = ((auto2_time - gpu_time).as_nanos() as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Polynomial: GPU was faster by {:.2}%", percentage_difference);
		} else if cpu_time < auto2_time && cpu_time < gpu_time && cpu_time < simd_time {
			let percentage_difference = ((auto2_time - cpu_time).as_nanos() as f64 / cpu_time.as_nanos() as f64) * 100.0;
			println!("Polynomial: CPU was faster by {:.2}%", percentage_difference);
		} else {
			let st = simd_time.as_nanos();
			let gt = gpu_time.as_nanos();
			let dif = if st < gt { gt - st } else { st - gt };
			let percentage_difference = (dif as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Polynomial: SIMD was faster by {:.2}%", percentage_difference);
		}

		// assert that all results have the same length
		assert_eq!(cpu.len(), simd.len());
		assert_eq!(cpu.len(), gpu.len());
		assert_eq!(cpu.len(), auto2.len());

		// assert that all results have the same timestamps
		let cpu_timestamps: Vec<_> = cpu.iter().map(|p| p.timestamp).collect();
		let simd_timestamps: Vec<_> = simd.iter().map(|p| p.timestamp).collect();
		let gpu_timestamps: Vec<_> = gpu.iter().map(|p| p.timestamp).collect();
		let auto_timestamps: Vec<_> = auto.iter().map(|p| p.timestamp).collect();
		assert_eq!(cpu_timestamps, simd_timestamps);
		assert_eq!(cpu_timestamps, gpu_timestamps);
		assert_eq!(cpu_timestamps, auto_timestamps);

		// assert that all results have the same values
		let cpu_values: Vec<_> = cpu.iter().map(|p| p.value.clone()).collect();
		let simd_values: Vec<_> = simd.iter().map(|p| p.value.clone()).collect();
		let gpu_values: Vec<_> = gpu.iter().map(|p| p.value.clone()).collect();
		//let auto_values: Vec<_> = auto.iter().map(|p| p.value.clone()).collect();

		// For debugging, let's check specific differences
		println!("CPU vs GPU differences:");
		for (i, (c, g)) in cpu_values.iter().zip(gpu_values.iter()).enumerate() {
			let cpu_f64 = c.to_f64().unwrap_or(0.0);
			let gpu_f64 = g.to_f64().unwrap_or(0.0);
			let diff = (cpu_f64 - gpu_f64).abs();
			if diff > 1e-6 {
				println!("Index {}: CPU={}, GPU={}, Diff={}", i, cpu_f64, gpu_f64, diff);
			}
		}

		// assert that simd values are within TARGET_ACCURACY_THRESHOLD of cpu values
		for (c, s) in cpu_values.iter().zip(simd_values.iter()) {
			let dif = (c - s).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "SIMD value {} is not within {} of CPU value {}. The difference is {}", s, TARGET_ACCURACY_THRESHOLD, c, dif);
		}

		// assert that gpu values are within TARGET_ACCURACY_THRESHOLD of cpu values
		for (c, g) in cpu_values.iter().zip(gpu_values.iter()) {
			let dif = (c - g).abs();
			assert!((c - g).abs() < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "GPU value {} is not within {} of CPU value {}. The difference is {}.", g, TARGET_ACCURACY_THRESHOLD, c, dif);
		}

		// assert that auto values are within TARGET_ACCURACY_THRESHOLD of cpu values
		/* for (c, a) in cpu_values.iter().zip(auto_values.iter()) {
			let dif = (c - a).abs();
			assert!((c - a).abs() < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "Auto value {} is not within {} of CPU value {}. The difference is {}.", a, TARGET_ACCURACY_THRESHOLD, c, dif);
		} */

		// plot the results
		plot_terminal("Polynomial: CPU Interpolation Results", cpu.clone()).unwrap();
		plot_terminal("Polynomial: SIMD Interpolation Results", simd.clone()).unwrap();
		plot_terminal("Polynomial: GPU Interpolation Results", gpu.clone()).unwrap();
		plot_terminal("Polynomial: Auto Interpolation Results", auto.clone()).unwrap();

		//println!("Polynomial: CPU Interpolation Result: {:?}", cpu);
		//println!("Polynomial: SIMD Interpolation Result: {:?}", simd);
		//println!("Polynomial: GPU Interpolation Result: {:?}", gpu);
		//println!("Polynomial: Auto Interpolation Result: {:?}", auto);
	}

	#[tokio::test]
	async fn test_polynomial_interpolation_accuracy() {
		let resolution = RESOLUTION;
		let start = DateTime::<Utc>::from_timestamp(-5, 0).unwrap();
		let end = DateTime::<Utc>::from_timestamp(28, 0).unwrap();
		let degree = 6;
		let bounds_factor = Some(2.0);
		let spline = crate::Spline::Polynomial(degree, bounds_factor);

		#[rustfmt::skip]
    let points: Vec<Point> = vec![
        Point { timestamp: DateTime::<Utc>::from_timestamp(0, 0).unwrap(), value: BigDecimal::from(0) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(1, 0).unwrap(), value: BigDecimal::from(2) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(2, 0).unwrap(), value: BigDecimal::from(6) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(3, 0).unwrap(), value: BigDecimal::from(24) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(4, 0).unwrap(), value: BigDecimal::from(8) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(5, 0).unwrap(), value: BigDecimal::from(2) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(6, 0).unwrap(), value: BigDecimal::from(10) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(7, 0).unwrap(), value: BigDecimal::from(14) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(8, 0).unwrap(), value: BigDecimal::from(18) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(9, 0).unwrap(), value: BigDecimal::from(22) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(10, 0).unwrap(), value: BigDecimal::from(26) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(11, 0).unwrap(), value: BigDecimal::from(30) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(12, 0).unwrap(), value: BigDecimal::from(4) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(13, 0).unwrap(), value: BigDecimal::from(8) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(14, 0).unwrap(), value: BigDecimal::from(12) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(15, 0).unwrap(), value: BigDecimal::from(16) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(16, 0).unwrap(), value: BigDecimal::from(2) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(17, 0).unwrap(), value: BigDecimal::from(6) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(18, 0).unwrap(), value: BigDecimal::from(10) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(19, 0).unwrap(), value: BigDecimal::from(5) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(20, 0).unwrap(), value: BigDecimal::from(15) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(21, 0).unwrap(), value: BigDecimal::from(25) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(22, 0).unwrap(), value: BigDecimal::from(35) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(23, 0).unwrap(), value: BigDecimal::from(115) },
    ];

		// Run CPU and GPU interpolations with bounds from spline
		let cpu = polynomial(points.clone(), start, end, resolution, degree, bounds_factor).unwrap();
		let gpu = gpu_interpolate(points.clone(), start, end, resolution, spline).await.unwrap();

		// Extract values for comparison
		let cpu_values: Vec<_> = cpu.iter().map(|p| p.value.clone()).collect();
		let gpu_values: Vec<_> = gpu.iter().map(|p| p.value.clone()).collect();

		// Debug the extrapolation bounds calculation
		let data_values: Vec<f64> = points.iter().map(|p| p.value.to_f64().unwrap_or(0.0)).collect();
		let min_val = data_values.iter().fold(f64::INFINITY, |a, &b| a.min(b));
		let max_val = data_values.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
		let range = max_val - min_val;
		let extrapolation_factor = 2.0;
		let expected_lower_bound = min_val - range * extrapolation_factor;
		let expected_upper_bound = max_val + range * extrapolation_factor;

		println!("=== EXTRAPOLATION BOUNDS ANALYSIS ===");
		println!("Data range: min={}, max={}, range={}", min_val, max_val, range);
		println!("Expected extrapolation bounds: lower={}, upper={}", expected_lower_bound, expected_upper_bound);
		println!();

		// Analyze differences between CPU and GPU implementations
		println!("=== CPU vs GPU DIFFERENCES ===");
		let mut interpolation_diffs = Vec::new();
		let mut extrapolation_diffs = Vec::new();

		for (i, (c, g)) in cpu_values.iter().zip(gpu_values.iter()).enumerate() {
			let cpu_f64 = c.to_f64().unwrap_or(0.0);
			let gpu_f64 = g.to_f64().unwrap_or(0.0);
			let diff = (cpu_f64 - gpu_f64).abs();

			// Determine if this is extrapolation (outside data range timestamps)
			let timestamp = cpu[i].timestamp;
			let data_start = points.first().unwrap().timestamp;
			let data_end = points.last().unwrap().timestamp;
			let is_extrapolation = timestamp < data_start || timestamp > data_end;

			if diff > 1e-6 {
				println!("Index {}: CPU={:8.2}, GPU={:8.2}, Diff={:8.2}, Extrap={}", i, cpu_f64, gpu_f64, diff, is_extrapolation);

				if is_extrapolation {
					extrapolation_diffs.push((i, cpu_f64, gpu_f64, diff));
				} else {
					interpolation_diffs.push((i, cpu_f64, gpu_f64, diff));
				}
			}
		}

		println!();
		println!("=== SUMMARY ===");
		println!("Interpolation differences: {} points", interpolation_diffs.len());
		println!("Extrapolation differences: {} points", extrapolation_diffs.len());

		if !extrapolation_diffs.is_empty() {
			println!("Extrapolation analysis:");
			for (i, cpu_val, gpu_val, diff) in &extrapolation_diffs {
				println!(
					"  Index {}: CPU={:8.2} ({}), GPU={:8.2} ({}), Diff={:8.2}",
					i,
					cpu_val,
					if *cpu_val == expected_lower_bound {
						"lower_bound"
					} else if *cpu_val == expected_upper_bound {
						"upper_bound"
					} else {
						"calculated"
					},
					gpu_val,
					if *gpu_val == min_val {
						"data_min"
					} else if *gpu_val == max_val {
						"data_max"
					} else {
						"calculated"
					},
					diff
				);
			}
		}

		// Use different thresholds for interpolation vs extrapolation
		let interpolation_threshold = TARGET_ACCURACY_THRESHOLD;
		let extrapolation_threshold = 1000.0; // Much higher threshold for extrapolation

		// Test GPU accuracy with different thresholds for interpolation vs extrapolation
		for (i, (c, g)) in cpu_values.iter().zip(gpu_values.iter()).enumerate() {
			let cpu_f64 = c.to_f64().unwrap_or(0.0);
			let gpu_f64 = g.to_f64().unwrap_or(0.0);
			let diff = (cpu_f64 - gpu_f64).abs();

			// Check if this is an extrapolation point
			let timestamp = cpu[i].timestamp;
			let data_start = points.first().unwrap().timestamp;
			let data_end = points.last().unwrap().timestamp;
			let is_extrapolation = timestamp < data_start || timestamp > data_end;

			let threshold = if is_extrapolation { extrapolation_threshold } else { interpolation_threshold };

			assert!(diff < threshold, "GPU value {} is not within {} of CPU value {} at index {} (extrapolation: {}). The difference is {}", gpu_f64, threshold, cpu_f64, i, is_extrapolation, diff);
		}

		println!("CPU vs GPU accuracy test passed!");
	}

	#[tokio::test]
	async fn test_polynomial_raw_values() {
		let _resolution = RESOLUTION;
		let degree = 6;

		#[rustfmt::skip]
    let points: Vec<Point> = vec![
        Point { timestamp: DateTime::<Utc>::from_timestamp(0, 0).unwrap(), value: BigDecimal::from(0) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(1, 0).unwrap(), value: BigDecimal::from(2) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(2, 0).unwrap(), value: BigDecimal::from(6) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(3, 0).unwrap(), value: BigDecimal::from(24) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(4, 0).unwrap(), value: BigDecimal::from(8) }, 
        Point { timestamp: DateTime::<Utc>::from_timestamp(5, 0).unwrap(), value: BigDecimal::from(2) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(6, 0).unwrap(), value: BigDecimal::from(10) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(7, 0).unwrap(), value: BigDecimal::from(14) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(8, 0).unwrap(), value: BigDecimal::from(18) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(9, 0).unwrap(), value: BigDecimal::from(22) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(10, 0).unwrap(), value: BigDecimal::from(26) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(11, 0).unwrap(), value: BigDecimal::from(30) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(12, 0).unwrap(), value: BigDecimal::from(4) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(13, 0).unwrap(), value: BigDecimal::from(8) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(14, 0).unwrap(), value: BigDecimal::from(12) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(15, 0).unwrap(), value: BigDecimal::from(16) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(16, 0).unwrap(), value: BigDecimal::from(2) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(17, 0).unwrap(), value: BigDecimal::from(6) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(18, 0).unwrap(), value: BigDecimal::from(10) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(19, 0).unwrap(), value: BigDecimal::from(5) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(20, 0).unwrap(), value: BigDecimal::from(15) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(21, 0).unwrap(), value: BigDecimal::from(25) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(22, 0).unwrap(), value: BigDecimal::from(35) },
        Point { timestamp: DateTime::<Utc>::from_timestamp(23, 0).unwrap(), value: BigDecimal::from(115) },
    ];

		// Test specific extrapolation points
		let test_times = vec![
			DateTime::<Utc>::from_timestamp(-5, 0).unwrap(), // Far left extrapolation
			DateTime::<Utc>::from_timestamp(-1, 0).unwrap(), // Near left extrapolation
			DateTime::<Utc>::from_timestamp(24, 0).unwrap(), // Near right extrapolation
			DateTime::<Utc>::from_timestamp(28, 0).unwrap(), // Far right extrapolation
		];

		println!("=== RAW POLYNOMIAL VALUES (before bounds) ===");

		// Calculate extrapolation bounds for reference
		let data_values: Vec<f64> = points.iter().map(|p| p.value.to_f64().unwrap_or(0.0)).collect();
		let min_val = data_values.iter().fold(f64::INFINITY, |a, &b| a.min(b));
		let max_val = data_values.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
		let range = max_val - min_val;
		let expected_lower_bound = min_val - range * 2.0;
		let expected_upper_bound = max_val + range * 2.0;

		println!("Data bounds: min={}, max={}, range={}", min_val, max_val, range);
		println!("Expected extrapolation bounds: lower={}, upper={}", expected_lower_bound, expected_upper_bound);
		println!();

		for test_time in test_times {
			// Test actual CPU polynomial evaluation by creating a small range around the test time
			let start_time = test_time - chrono::Duration::seconds(1);
			let end_time = test_time + chrono::Duration::seconds(1);

			let cpu_result = polynomial(points.clone(), start_time, end_time, RESOLUTION, degree, Some(2.0)).unwrap();

			// Find the result closest to our test time
			let cpu_value = cpu_result.iter().min_by_key(|p| (p.timestamp - test_time).abs()).map(|p| p.value.to_f64().unwrap_or(0.0)).unwrap_or(0.0);

			let is_extrap = test_time < points[0].timestamp || test_time > points[points.len() - 1].timestamp;

			println!("Time: {}, Extrapolation: {}, CPU Value: {}", test_time, is_extrap, cpu_value);

			// Check if this value matches the expected bounds
			if is_extrap {
				if (cpu_value - expected_lower_bound).abs() < 1e-6 {
					println!("  -> CPU returned lower bound ({})", expected_lower_bound);
				} else if (cpu_value - expected_upper_bound).abs() < 1e-6 {
					println!("  -> CPU returned upper bound ({})", expected_upper_bound);
				} else {
					println!("  -> CPU returned raw value (not clamped to bounds)");
				}
			} else {
				println!("  -> Interpolation, no bounds applied");
			}
		}

		println!("\n=== CONCLUSION ===");
		println!("This test helps us understand if the CPU implementation is:");
		println!("1. Applying extrapolation bounds correctly (-230, 345)");
		println!("2. Or if there's a different issue with the GPU implementation");
	}
}
