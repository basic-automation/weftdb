use std::{hint::black_box, str::FromStr};

use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, Criterion};
use database::*;
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

fn benchmark_strategy_selection(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let dataset_sizes = vec![("small_dataset", 100, Resolution::Minutes), ("medium_dataset", 1000, Resolution::Seconds), ("large_dataset", 10000, Resolution::Seconds)];

	for (name, size, resolution) in dataset_sizes {
		c.bench_function(name, |b| {
			b.iter(|| {
				rt.block_on(async {
					// Create unique database for each iteration
					let db_name = format!("bench_strategy_{}_{}", name, Uuid::new_v4());
					let db_path = format!("data/{db_name}");
					std::fs::remove_dir_all(&db_path).ok();
					tokio::time::sleep(std::time::Duration::from_millis(200)).await;

					let db = Database::new(&db_name).await.unwrap();
					let subject = db.track_subject("strategy_subject").await.unwrap();
					let aspect = db.track_aspect(subject, "strategy_aspect").await.unwrap();

					// Generate and insert test data
					let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
					let mut measurements = Vec::with_capacity(size);
					for i in 0..size {
						measurements.push(InputMeasurement::new(base_time + Duration::seconds(i as i64), BigDecimal::from_str(&format!("{}.0", i)).unwrap()));
					}
					db.observe_measurements_batch(aspect.clone(), measurements).await.unwrap();

					// Perform analysis
					let start = base_time;
					let end = base_time + Duration::seconds((size - 1) as i64);
					let result = Database::analyze_range(aspect.id(), start, end, resolution, Spline::Linear).await.unwrap();

					// Cleanup
					db.close().await.unwrap();
					tokio::time::sleep(std::time::Duration::from_millis(200)).await;
					std::fs::remove_dir_all(&db_path).ok();

					black_box(result)
				})
			});
		});
	}
}

criterion_group!(benches, benchmark_strategy_selection);
criterion_main!(benches);
