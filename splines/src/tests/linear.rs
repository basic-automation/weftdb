#[cfg(test)]
mod tests {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{DateTime, Utc};

	use super::super::plot_terminal;
	use crate::{auto_interpolate, generate_target_times, gpu_interpolate, linear, simd_interpolate, Point, Resolution, parallel_simd_interpolate};
	use std::sync::LazyLock;

	const TARGET_ACCURACY_THRESHOLD: f64 = 10.0e-1;
	const RESOLUTION: Resolution = Resolution::Microseconds;
	const START: DateTime<Utc> = DateTime::<Utc>::from_timestamp(-4, 0).unwrap();
	const END: DateTime<Utc> = DateTime::<Utc>::from_timestamp(35, 0).unwrap();
	const SPLINE: crate::Spline = crate::Spline::Linear;
	#[rustfmt::skip]
	static POINTS: LazyLock<[Point; 24]> = LazyLock::new(|| [
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
		Point { timestamp: DateTime::<Utc>::from_timestamp(26, 0).unwrap(), value: BigDecimal::from(35) },
		Point { timestamp: DateTime::<Utc>::from_timestamp(30, 0).unwrap(), value: BigDecimal::from(115) },
	]);

	#[tokio::test]
	async fn test_linear_interpolation() {
		let target_times = generate_target_times(START, END, RESOLUTION);

		let input_count = POINTS.len();
		let output_count = target_times.len();
		println!("Linear: Input count: {}, Output count: {}", input_count, output_count);

		// cpu interpolation
		let timer = tokio::time::Instant::now();
		let cpu = linear(POINTS.clone().to_vec(), START, END, RESOLUTION).unwrap();
		let cpu_time = timer.elapsed();
		println!("Linear: CPU Interpolation took: {:?}", cpu_time);

		// simd interpolation
		let timer = tokio::time::Instant::now();
		let simd = simd_interpolate(&POINTS.to_vec(), &target_times, SPLINE, RESOLUTION).unwrap();
		let simd_time = timer.elapsed();
		println!("Linear: SIMD Interpolation took: {:?}", simd_time);

		// parallel SIMD interpolation
		let timer = tokio::time::Instant::now();
		let parallel_simd = parallel_simd_interpolate(&POINTS.to_vec(), &target_times, SPLINE, RESOLUTION).unwrap();
		let parallel_simd_time = timer.elapsed();
		println!("Linear: Parallel SIMD Interpolation took: {:?}", parallel_simd_time);

		// gpu interpolation
		let timer = tokio::time::Instant::now();
		let gpu = gpu_interpolate(POINTS.clone().to_vec(), target_times, SPLINE, RESOLUTION).await.unwrap();
		let gpu_time = timer.elapsed();
		println!("Linear: GPU Interpolation took: {:?}", gpu_time);

		// automatically determine the fastest method
		let auto_timer = tokio::time::Instant::now();
		let auto = auto_interpolate(POINTS.clone().to_vec(), START, END, RESOLUTION, SPLINE).await.unwrap();
		let auto_time = auto_timer.elapsed();
		println!("Linear: Auto Interpolation took: {:?}", auto_time);

		if auto_time < gpu_time && auto_time < cpu_time && auto_time < simd_time && auto_time < parallel_simd_time {
			let percentage_difference = ((gpu_time - auto_time).as_nanos() as f64 / auto_time.as_nanos() as f64) * 100.0;
			println!("Linear: Auto was faster by {:.2}%", percentage_difference);
		} else if gpu_time < auto_time && gpu_time < cpu_time && gpu_time < simd_time && gpu_time < parallel_simd_time {
			let percentage_difference = ((auto_time - gpu_time).as_nanos() as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Linear: GPU was faster by {:.2}%", percentage_difference);
		} else if cpu_time < auto_time && cpu_time < gpu_time && cpu_time < simd_time && cpu_time < parallel_simd_time {
			let percentage_difference = ((auto_time - cpu_time).as_nanos() as f64 / cpu_time.as_nanos() as f64) * 100.0;
			println!("Linear: CPU was faster by {:.2}%", percentage_difference);
		} else if simd_time < auto_time && simd_time < gpu_time && simd_time < cpu_time && simd_time < parallel_simd_time {
			let percentage_difference = ((auto_time - simd_time).as_nanos() as f64 / simd_time.as_nanos() as f64) * 100.0;
			println!("Linear: SIMD was faster by {:.2}%", percentage_difference);
		} else {
			let st = parallel_simd_time.as_nanos();
			let gt = gpu_time.as_nanos();
			let dif = if st < gt { gt - st } else { st - gt };
			let percentage_difference = (dif as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Linear: Parallel SIMD was faster by {:.2}%", percentage_difference);
		}

		// assert that all results have the same length
		assert_eq!(cpu.len(), simd.len());
		assert_eq!(cpu.len(), gpu.len());
		assert_eq!(cpu.len(), parallel_simd.len());
		assert_eq!(cpu.len(), auto.len());

		// assert that all results have the same timestamps
		let cpu_timestamps: Vec<_> = cpu.iter().map(|p| p.timestamp).collect();
		let simd_timestamps: Vec<_> = simd.iter().map(|p| p.timestamp).collect();
		let gpu_timestamps: Vec<_> = gpu.iter().map(|p| p.timestamp).collect();
		let parallel_simd_timestamps: Vec<_> = parallel_simd.iter().map(|p| p.timestamp).collect();
		let auto_timestamps: Vec<_> = auto.iter().map(|p| p.timestamp).collect();
		assert_eq!(cpu_timestamps, simd_timestamps);
		assert_eq!(cpu_timestamps, gpu_timestamps);
		assert_eq!(cpu_timestamps, parallel_simd_timestamps);
		assert_eq!(cpu_timestamps, auto_timestamps);

		// assert that all results have the same values
		let cpu_values: Vec<_> = cpu.iter().map(|p| p.value.clone()).collect();
		let simd_values: Vec<_> = simd.iter().map(|p| p.value.clone()).collect();
		let gpu_values: Vec<_> = gpu.iter().map(|p| p.value.clone()).collect();
		let parallel_simd_values: Vec<_> = parallel_simd.iter().map(|p| p.value.clone()).collect();
		let auto_values: Vec<_> = auto.iter().map(|p| p.value.clone()).collect();

		// assert that simd values are within TARGET_ACCURACY_THRESHOLD of cpu values
		for (c, s) in cpu_values.iter().zip(simd_values.iter()) {
			assert!((c - s).abs() < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "SIMD value {} is not within {} of CPU value {}. The difference is {}.", s, TARGET_ACCURACY_THRESHOLD, c, (c - s).abs());
		}

		// assert that gpu values are within TARGET_ACCURACY_THRESHOLD of cpu values
		for (c, g) in cpu_values.iter().zip(gpu_values.iter()) {
			let dif = (c - g).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "GPU value {} is not within {} of CPU value {}. The difference is {}.", g, TARGET_ACCURACY_THRESHOLD, c, dif);
		}

		// assert that parallel SIMD values are within TARGET_ACCURACY_THRESHOLD of cpu values
		for (c, p) in cpu_values.iter().zip(parallel_simd_values.iter()) {
			let dif = (c - p).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "Parallel SIMD value {} is not within {} of CPU value {}. The difference is {}", p, TARGET_ACCURACY_THRESHOLD, c, dif);
		}

		// assert that auto values are within TARGET_ACCURACY_THRESHOLD of cpu values
		for (c, a) in cpu_values.iter().zip(auto_values.iter()) {
			let dif = (c - a).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "Auto value {} is not within {} of CPU value {}. The difference is {}.", a, TARGET_ACCURACY_THRESHOLD, c, dif);
		}

		// plot the results
		plot_terminal("Linear: CPU Interpolation Results", cpu.clone()).unwrap();
		plot_terminal("Linear: SIMD Interpolation Results", simd.clone()).unwrap();
		plot_terminal("Linear: GPU Interpolation Results", gpu.clone()).unwrap();
		plot_terminal("Linear: Parallel SIMD Interpolation Results", parallel_simd.clone()).unwrap();
		plot_terminal("Linear: Auto Interpolation Results", auto.clone()).unwrap();

		//println!("Linear: CPU Interpolation Result: {:?}", cpu);
		//println!("Linear: SIMD Interpolation Result: {:?}", simd);
		//println!("Linear: GPU Interpolation Result: {:?}", gpu);
		//println!("Linear: Parallel SIMD Interpolation Result: {:?}", parallel_simd);
		//println!("Linear: Auto Interpolation Result: {:?}", auto);
	}
}
