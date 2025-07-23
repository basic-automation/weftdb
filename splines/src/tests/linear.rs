#[cfg(test)]
mod tests {
	use std::sync::LazyLock;

	use bigdecimal::{BigDecimal, FromPrimitive};
	use chrono::{DateTime, Utc};

	use super::super::plot_terminal;
	use crate::{Point, Resolution, TargetTimesIterator, auto_interpolate, gpu_interpolate, linear, parallel_interpolate};

	const TARGET_ACCURACY_THRESHOLD: f64 = 10.0e-1;
	const RESOLUTION: Resolution = Resolution::Nanoseconds;
	const START: DateTime<Utc> = DateTime::<Utc>::from_timestamp(-4, 0).unwrap();
	const END: DateTime<Utc> = DateTime::<Utc>::from_timestamp(-3, 0).unwrap();
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
		let target_times_iter = TargetTimesIterator::new(START, END, RESOLUTION);

		let input_count = POINTS.len();
		let output_count = target_times_iter.estimate_len().unwrap();
		println!("Linear: Input count: {}, Output count: {}", input_count, output_count);

		// cpu interpolation
		println!("Linear: CPU Test Starting...");
		let timer = tokio::time::Instant::now();
		let cpu = linear(&POINTS.to_vec(), &START, &END, &RESOLUTION).unwrap();
		let cpu_time = timer.elapsed();
		println!("Linear: CPU Interpolation took: {:?}", cpu_time);
		let cpu_len = cpu.len();
		let (cpu_timestamps, cpu_values): (Vec<_>, Vec<_>) = cpu.iter().map(|p| (p.timestamp, p.value.clone())).unzip();
		drop(cpu);

		// parallel interpolation
		println!("Linear: Parallel Test Starting...");
		let timer = tokio::time::Instant::now();
		let parallel = parallel_interpolate(&POINTS.to_vec(), &START, &END, SPLINE, RESOLUTION).unwrap();
		let parallel_time = timer.elapsed();
		println!("Linear: Parallel Interpolation took: {:?}", parallel_time);
		let parallel_len = parallel.len();
		let (parallel_timestamps, parallel_values): (Vec<_>, Vec<_>) = parallel.iter().map(|p| (p.timestamp, p.value.clone())).unzip();
		drop(parallel);

		// compare cpu and parallel results
		println!("Linear: Comparing CPU and Parallel results...");
		assert_eq!(cpu_len, parallel_len);
		for (c, s) in cpu_values.iter().zip(parallel_values.iter()) {
			assert!((c - s).abs() < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "PARALLEL value {} is not within {} of CPU value {}. The difference is {}.", s, TARGET_ACCURACY_THRESHOLD, c, (c - s).abs());
		}
		assert_eq!(cpu_timestamps, parallel_timestamps);
		drop(parallel_timestamps);
		drop(parallel_values);

		// gpu interpolation
		println!("Linear: GPU Test Starting...");
		let timer = tokio::time::Instant::now();
		let gpu = gpu_interpolate(POINTS.clone().to_vec(), START, END, RESOLUTION, SPLINE).await.unwrap();
		let gpu_time = timer.elapsed();
		println!("Linear: GPU Interpolation took: {:?}", gpu_time);
		let gpu_len = gpu.len();
		let (gpu_timestamps, gpu_values): (Vec<_>, Vec<_>) = gpu.iter().map(|p| (p.timestamp, p.value.clone())).unzip();
		drop(gpu);

		// compare cpu and gpu results
		println!("Linear: Comparing CPU and GPU results...");
		assert_eq!(cpu_len, gpu_len);
		for (c, g) in cpu_values.iter().zip(gpu_values.iter()) {
			let dif = (c - g).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "GPU value {} is not within {} of CPU value {}. The difference is {}.", g, TARGET_ACCURACY_THRESHOLD, c, dif);
		}
		assert_eq!(cpu_timestamps, gpu_timestamps);
		drop(gpu_timestamps);
		drop(gpu_values);

		// automatically determine the fastest method
		println!("Linear: Auto Test Starting...");
		let auto_timer = tokio::time::Instant::now();
		let auto = auto_interpolate(POINTS.clone().to_vec(), START, END, RESOLUTION, SPLINE).await.unwrap();
		let auto_time = auto_timer.elapsed();
		println!("Linear: Auto Interpolation took: {:?}", auto_time);
		let auto_len = auto.len();
		let (auto_timestamps, auto_values): (Vec<_>, Vec<_>) = auto.iter().map(|p| (p.timestamp, p.value.clone())).unzip();
		drop(auto);

		// compare cpu and auto results
		println!("Linear: Comparing CPU and Auto results...");
		assert_eq!(cpu_len, auto_len);
		for (c, a) in cpu_values.iter().zip(auto_values.iter()) {
			let dif = (c - a).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "Auto value {} is not within {} of CPU value {}. The difference is {}.", a, TARGET_ACCURACY_THRESHOLD, c, dif);
		}
		assert_eq!(cpu_timestamps, auto_timestamps);
		drop(auto_timestamps);
		drop(auto_values);

		if auto_time < gpu_time && auto_time < cpu_time && auto_time < parallel_time && auto_time < parallel_time {
			let percentage_difference = ((gpu_time - auto_time).as_nanos() as f64 / auto_time.as_nanos() as f64) * 100.0;
			println!("Linear: Auto was faster by {:.2}%", percentage_difference);
		} else if gpu_time < auto_time && gpu_time < cpu_time && gpu_time < parallel_time && gpu_time < parallel_time {
			let percentage_difference = ((auto_time - gpu_time).as_nanos() as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Linear: GPU was faster by {:.2}%", percentage_difference);
		} else if cpu_time < auto_time && cpu_time < gpu_time && cpu_time < parallel_time && cpu_time < parallel_time {
			let percentage_difference = ((auto_time - cpu_time).as_nanos() as f64 / cpu_time.as_nanos() as f64) * 100.0;
			println!("Linear: CPU was faster by {:.2}%", percentage_difference);
		} else if parallel_time < auto_time && parallel_time < gpu_time && parallel_time < cpu_time && parallel_time < parallel_time {
			let percentage_difference = ((auto_time - parallel_time).as_nanos() as f64 / parallel_time.as_nanos() as f64) * 100.0;
			println!("Linear: SIMD was faster by {:.2}%", percentage_difference);
		} else {
			let st = parallel_time.as_nanos();
			let gt = gpu_time.as_nanos();
			let dif = if st < gt { gt - st } else { st - gt };
			let percentage_difference = (dif as f64 / gpu_time.as_nanos() as f64) * 100.0;
			println!("Linear: Parallel SIMD was faster by {:.2}%", percentage_difference);
		}

		// assert that all results have the same values
		/* let cpu_values: Vec<_> = cpu.iter().map(|p| p.value.clone()).collect(); */
		/* let parallel_values: Vec<_> = parallel.iter().map(|p| p.value.clone()).collect();
		/* let gpu_values: Vec<_> = gpu.iter().map(|p| p.value.clone()).collect(); */
		let auto_values: Vec<_> = auto.iter().map(|p| p.value.clone()).collect(); */

		// assert that simd values are within TARGET_ACCURACY_THRESHOLD of cpu values
		/* for (c, s) in cpu_values.iter().zip(parallel_values.iter()) {
			assert!((c - s).abs() < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "PARALLEL value {} is not within {} of CPU value {}. The difference is {}.", s, TARGET_ACCURACY_THRESHOLD, c, (c - s).abs());
		} */

		// assert that auto values are within TARGET_ACCURACY_THRESHOLD of cpu values
		/* for (c, a) in cpu_values.iter().zip(auto_values.iter()) {
			let dif = (c - a).abs();
			assert!(dif < BigDecimal::from_f64(TARGET_ACCURACY_THRESHOLD).unwrap(), "Auto value {} is not within {} of CPU value {}. The difference is {}.", a, TARGET_ACCURACY_THRESHOLD, c, dif);
		} */

		// plot the results
		/* plot_terminal("Linear: CPU Interpolation Results", cpu.clone()).unwrap(); */
		/* plot_terminal("Linear: PARALLEL Interpolation Results", parallel.clone()).unwrap(); */
		/* plot_terminal("Linear: GPU Interpolation Results", gpu.clone()).unwrap(); */
		/* plot_terminal("Linear: Auto Interpolation Results", auto.clone()).unwrap(); */

		//println!("Linear: CPU Interpolation Result: {:?}", cpu);
		//println!("Linear: PARALLEL Interpolation Result: {:?}", simd);
		//println!("Linear: GPU Interpolation Result: {:?}", gpu);
		//println!("Linear: Auto Interpolation Result: {:?}", auto);
	}
}
