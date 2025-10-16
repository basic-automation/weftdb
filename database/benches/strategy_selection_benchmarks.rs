use std::{hint::black_box, str::FromStr};

use ::database::{
	database::traits::{DatabaseStructure, Inputs, Outputs}, Database, InputMeasurement
};
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_strategy_selection(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Further reduced dataset sizes for faster benchmarks
	let dataset_sizes = vec![
		("small_dataset", 50, Resolution::Minutes),   // Reduced from 100 to 50
		("medium_dataset", 100, Resolution::Minutes), // Reduced from 500 to 100, changed to Minutes
		("large_dataset", 200, Resolution::Minutes),  // Reduced from 1000 to 200, changed to Minutes
	];

	// Configure criterion for slow benchmarks
	let mut group = c.benchmark_group("strategy_selection");
	group.sample_size(10); // Minimum 10 samples required by Criterion
	group.measurement_time(std::time::Duration::from_secs(16)); // Increased to address 14.3-14.4s warnings
	group.warm_up_time(std::time::Duration::from_secs(3)); // Warm-up for database ops

	for (name, size, resolution) in dataset_sizes {
		group.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					// Create unique database for each iteration
					let db_name = format!("bench_strategy_{}_{}", name, Uuid::new_v4());
					let db_path = format!("data/{db_name}");
					std::fs::remove_dir_all(&db_path).ok();
					tokio::time::sleep(std::time::Duration::from_millis(100)).await;

					let db = Database::new(&db_name).await.unwrap();
					let subject = db.observe_subject("strategy_subject").await.unwrap();
					let aspect = db.track_aspect(subject.id(), "strategy_aspect", resolution).await.unwrap();

					// Generate test data using batch insertion for efficiency
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let mut measurements = Vec::with_capacity(size);
					for i in 0..size {
						measurements.push(InputMeasurement::new(base_time + Duration::minutes(i as i64), BigDecimal::from_str(&format!("{}.5", i + 10)).unwrap()));
					}

					// Use batch insertion instead of individual insertions
					db.batch_capture_measurements(aspect.clone(), measurements).await.unwrap();

					// Perform interpolation analysis
					let analyze_time = base_time + Duration::minutes((size / 2) as i64);
					let result = db.analyze_point(aspect.id(), analyze_time, resolution, Spline::Linear).await.unwrap();

					// Cleanup
					db.close().await.unwrap();

					// Add small delay before cleanup
					tokio::time::sleep(std::time::Duration::from_millis(100)).await;
					std::fs::remove_dir_all(&db_path).ok();

					black_box(result)
				})
			});
		});
	}
	group.finish();
}

criterion_group!(benches, benchmark_strategy_selection);
criterion_main!(benches);
