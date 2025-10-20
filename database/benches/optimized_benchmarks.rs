use std::{hint::black_box, str::FromStr, sync::Arc};

use ::database::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, DatasetId, *
};
use bigdecimal::BigDecimal;
use chrono::{Duration, TimeZone, Utc};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use splimes::{Resolution, Spline};
use tokio::runtime::Runtime;
use uuid::Uuid;

// Shared benchmark infrastructure to reduce setup overhead
struct BenchmarkContext {
	db: Arc<Database>,
	aspect_id: AspectId,
	_cleanup_path: String,
}

impl BenchmarkContext {
	async fn new(name: &str, measurement_count: usize) -> anyhow::Result<Self> {
		let db_name = format!("bench_{}_{}", name, Uuid::new_v4());
		let cleanup_path = format!("data/{db_name}");

		// Clean up any existing data
		std::fs::remove_dir_all(&cleanup_path).ok();
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let db = Arc::new(Database::new(&db_name).await?);
		let subject = db.observe_subject("bench_subject").await?;
		let aspect = db.track_aspect(subject.id(), "bench_aspect", Resolution::Seconds).await?;

		// Add test measurements
		let base_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		for i in 0..measurement_count {
			let measurement = InputMeasurement::new(base_time + Duration::seconds(i as i64 * 60), BigDecimal::from_str(&format!("{}.0", i + 10))?);
			db.capture_measurement(aspect.id(), DatasetId::new(), measurement).await?;
		}

		Ok(Self { db, aspect_id: aspect.id(), _cleanup_path: cleanup_path })
	}
}

impl Drop for BenchmarkContext {
	fn drop(&mut self) {
		std::fs::remove_dir_all(&self._cleanup_path).ok();
	}
}

fn benchmark_optimized_interpolation(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	// Create contexts once and reuse them
	let contexts = vec![("small", 100), ("medium", 500), ("large", 1000)];

	for (name, size) in contexts {
		c.bench_function(&format!("optimized_interpolation_{name}"), |b| {
			b.iter_batched(
				|| rt.block_on(BenchmarkContext::new(name, size)).unwrap(),
				|ctx| {
					rt.block_on(async {
						let analyze_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 30, 0).unwrap();
						let result = ctx.db.analyze_point(ctx.aspect_id, analyze_time, Resolution::Seconds, Spline::Linear).await.unwrap();
						black_box(result)
					})
				},
				BatchSize::SmallInput,
			);
		});
	}
}

fn benchmark_cache_efficiency(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	c.bench_function("cache_hit_ratio", |b| {
		b.iter_batched(
			|| rt.block_on(BenchmarkContext::new("cache_efficiency", 100)).unwrap(),
			|ctx| {
				rt.block_on(async {
					let analyze_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 15, 0).unwrap();
					// First call to cache the result
					let _ = ctx.db.analyze_point(ctx.aspect_id, analyze_time, Resolution::Seconds, Spline::Linear).await.unwrap();
					// Second call should hit cache
					let result = ctx.db.analyze_point(ctx.aspect_id, analyze_time, Resolution::Seconds, Spline::Linear).await.unwrap();
					black_box(result)
				})
			},
			BatchSize::SmallInput,
		);
	});
}

criterion_group!(benches, benchmark_optimized_interpolation, benchmark_cache_efficiency);
criterion_main!(benches);
