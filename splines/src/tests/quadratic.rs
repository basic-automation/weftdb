#[cfg(test)]
mod tests {
	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{DateTime, Utc};

	use super::super::plot_terminal;
	use crate::{Point, Resolution, auto_interpolate, generate_target_times, gpu_interpolate, quadratic, quadratic_simd};

	const TARGET_ACCURACY_THRESHOLD: f64 = 1.0e2;
	const RESOLUTION: Resolution = Resolution::Seconds;

	#[tokio::test]
	async fn test_quadratic_interpolation() {
		let resolution = RESOLUTION;
		let start = DateTime::<Utc>::from_timestamp(-5, 0).unwrap();
		let end = DateTime::<Utc>::from_timestamp(28, 0).unwrap();
		let spline = crate::Spline::Quadratic;

		let points: Vec<Point> = vec![Point { timestamp: DateTime::<Utc>::from_timestamp(0, 0).unwrap(), value: BigDecimal::from(0) }, Point { timestamp: DateTime::<Utc>::from_timestamp(1, 0).unwrap(), value: BigDecimal::from(2) }, Point { timestamp: DateTime::<Utc>::from_timestamp(2, 0).unwrap(), value: BigDecimal::from(6) }, Point { timestamp: DateTime::<Utc>::from_timestamp(3, 0).unwrap(), value: BigDecimal::from(24) }, Point { timestamp: DateTime::<Utc>::from_timestamp(4, 0).unwrap(), value: BigDecimal::from(8) }, Point { timestamp: DateTime::<Utc>::from_timestamp(5, 0).unwrap(), value: BigDecimal::from(2) }, Point { timestamp: DateTime::<Utc>::from_timestamp(6, 0).unwrap(), value: BigDecimal::from(10) }, Point { timestamp: DateTime::<Utc>::from_timestamp(7, 0).unwrap(), value: BigDecimal::from(14) }, Point { timestamp: DateTime::<Utc>::from_timestamp(8, 0).unwrap(), value: BigDecimal::from(18) }, Point { timestamp: DateTime::<Utc>::from_timestamp(9, 0).unwrap(), value: BigDecimal::from(22) }, Point { timestamp: DateTime::<Utc>::from_timestamp(10, 0).unwrap(), value: BigDecimal::from(26) }, Point { timestamp: DateTime::<Utc>::from_timestamp(11, 0).unwrap(), value: BigDecimal::from(30) }, Point { timestamp: DateTime::<Utc>::from_timestamp(12, 0).unwrap(), value: BigDecimal::from(4) }, Point { timestamp: DateTime::<Utc>::from_timestamp(13, 0).unwrap(), value: BigDecimal::from(8) }, Point { timestamp: DateTime::<Utc>::from_timestamp(14, 0).unwrap(), value: BigDecimal::from(12) }, Point { timestamp: DateTime::<Utc>::from_timestamp(15, 0).unwrap(), value: BigDecimal::from(16) }, Point { timestamp: DateTime::<Utc>::from_timestamp(16, 0).unwrap(), value: BigDecimal::from(2) }, Point { timestamp: DateTime::<Utc>::from_timestamp(17, 0).unwrap(), value: BigDecimal::from(6) }, Point { timestamp: DateTime::<Utc>::from_timestamp(18, 0).unwrap(), value: BigDecimal::from(10) }, Point { timestamp: DateTime::<Utc>::from_timestamp(19, 0).unwrap(), value: BigDecimal::from(5) }, Point { timestamp: DateTime::<Utc>::from_timestamp(20, 0).unwrap(), value: BigDecimal::from(15) }, Point { timestamp: DateTime::<Utc>::from_timestamp(21, 0).unwrap(), value: BigDecimal::from(25) }, Point { timestamp: DateTime::<Utc>::from_timestamp(22, 0).unwrap(), value: BigDecimal::from(35) }, Point { timestamp: DateTime::<Utc>::from_timestamp(23, 0).unwrap(), value: BigDecimal::from(115) }];

		let target_times = generate_target_times(start, end, resolution);

		// cpu interpolation
		let timer = tokio::time::Instant::now();
		let cpu = quadratic(points.clone(), start, end, resolution).unwrap();
		let cpu_time = timer.elapsed();
		println!("Quadratic: CPU Interpolation took: {:?}", cpu_time);

		// simd interpolation
		let timer = tokio::time::Instant::now();
		let simd = quadratic_simd(&points, &target_times, resolution).unwrap();
		let simd_time = timer.elapsed();
		println!("Quadratic: SIMD Interpolation took: {:?}", simd_time);

		// gpu interpolation
		let timer = tokio::time::Instant::now();
		let gpu = gpu_interpolate(points.clone(), target_times, spline, resolution).await.unwrap();
		let gpu_time = timer.elapsed();
		println!("Quadratic: GPU Interpolation took: {:?}", gpu_time);

		// automatically determine the fastest method
		let auto_timer = tokio::time::Instant::now();
		let auto = auto_interpolate(points.clone(), start, end, resolution, spline).await.unwrap();
		let auto_time = auto_timer.elapsed();
		println!("Quadratic: Auto Interpolation took: {:?}", auto_time);

		if auto_time < gpu_time && auto_time < cpu_time && auto_time < simd_time {
			let percentage_difference = ((gpu_time - auto_time).as_nanos() as f64 / auto_time.as_nanos() as f64) * 100.0;
			println!("Quadratic: Auto was faster by {:.2}%", percentage_difference);
		} else if gpu_time < auto_time && gpu_time < cpu_time && gpu_time < simd_time {
			let percentage_difference = ((auto_time - gpu_time).as_nanos() as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Quadratic: GPU was faster by {:.2}%", percentage_difference);
		} else if cpu_time < auto_time && cpu_time < gpu_time && cpu_time < simd_time {
			let percentage_difference = ((auto_time - cpu_time).as_nanos() as f64 / cpu_time.as_nanos() as f64) * 100.0;
			println!("Quadratic: CPU was faster by {:.2}%", percentage_difference);
		} else {
			let st = simd_time.as_nanos();
			let gt = gpu_time.as_nanos();
			let dif = if st < gt { gt - st } else { st - gt };
			let percentage_difference = (dif as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Quadratic: SIMD was faster by {:.2}%", percentage_difference);
		}

		// assert that all results have the same length
		assert_eq!(cpu.len(), simd.len());
		assert_eq!(cpu.len(), gpu.len());
		assert_eq!(cpu.len(), auto.len());

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
		let auto_values: Vec<_> = auto.iter().map(|p| p.value.clone()).collect();

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
		for (c, a) in cpu_values.iter().zip(auto_values.iter()) {
			let dif = (c - a).abs();
			assert!((c - a).abs() < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "Auto value {} is not within {} of CPU value {}. The difference is {}.", a, TARGET_ACCURACY_THRESHOLD, c, dif);
		}

		// plot the results
		plot_terminal("Quadratic: CPU Interpolation Results", cpu.clone()).unwrap();
		plot_terminal("Quadratic: SIMD Interpolation Results", simd.clone()).unwrap();
		plot_terminal("Quadratic: GPU Interpolation Results", gpu.clone()).unwrap();
		plot_terminal("Quadratic: Auto Interpolation Results", auto.clone()).unwrap();

		//println!("Quadratic: CPU Interpolation Result: {:?}", cpu);
		//println!("Quadratic: SIMD Interpolation Result: {:?}", simd);
		//println!("Quadratic: GPU Interpolation Result: {:?}", gpu);
		//println!("Quadratic: Auto Interpolation Result: {:?}", auto);
	}
}
