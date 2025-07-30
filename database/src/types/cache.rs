use std::{
	collections::HashMap, sync::{Arc, LazyLock}
};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::types::Measurement;

pub static CACHE: LazyLock<DatabaseCache> = LazyLock::new(DatabaseCache::default);

#[derive(Debug, Clone)]
pub struct AnalysisResult {
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
	pub method: String,
	pub resolution: String,
}

#[derive(Debug, Default)]
pub struct DatabaseCache {
	measurements: Arc<RwLock<HashMap<String, Vec<Measurement>>>>,
	point_analysis: Arc<RwLock<HashMap<String, AnalysisResult>>>,
	range_analysis: Arc<RwLock<HashMap<String, Vec<AnalysisResult>>>>,
}

impl DatabaseCache {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	// Aspect measurement caching
	pub async fn get_aspect_measurements(&self, key: &str, _aspect_id: Uuid) -> Option<Vec<Measurement>> {
		let cache = self.measurements.read().await;
		cache.get(key).cloned()
	}

	pub async fn store_aspect_measurements(&self, key: &str, measurements: &[Measurement], _aspect_id: Uuid) {
		let mut cache = self.measurements.write().await;
		cache.insert(key.to_string(), measurements.to_vec());
	}

	// Point analysis caching
	pub async fn get_point_analysis(&self, key: &str) -> Option<AnalysisResult> {
		let cache = self.point_analysis.read().await;
		cache.get(key).cloned()
	}

	pub async fn store_point_analysis(&self, key: &str, result: &AnalysisResult) {
		let mut cache = self.point_analysis.write().await;
		cache.insert(key.to_string(), result.clone());
	}

	// Range analysis caching
	pub async fn get_range_analysis(&self, key: &str) -> Option<Vec<AnalysisResult>> {
		let cache = self.range_analysis.read().await;
		cache.get(key).cloned()
	}

	pub async fn store_range_analysis(&self, key: &str, results: &[AnalysisResult]) {
		let mut cache = self.range_analysis.write().await;
		cache.insert(key.to_string(), results.to_vec());
	}

	// Invalidate cache for a specific aspect
	pub async fn invalidate_aspect_cache(&self, cache_key: &str, aspect_id: Uuid) {
		let aspect_str = aspect_id.to_string();

		// Remove measurements
		{
			let mut cache = self.measurements.write().await;
			cache.remove(cache_key);
		}

		// Remove point analysis results
		{
			let mut cache = self.point_analysis.write().await;
			cache.retain(|key, _| !key.contains(&aspect_str));
		}

		// Remove range analysis results
		{
			let mut cache = self.range_analysis.write().await;
			cache.retain(|key, _| !key.contains(&aspect_str));
		}
	}

	// Keep backward compatibility methods
	pub async fn get_measurements(&self, subject_name: &str, dataset_id: Uuid) -> Option<Vec<Measurement>> {
		let key = format!("{subject_name}_{dataset_id}");
		let cache = self.measurements.read().await;
		cache.get(&key).cloned()
	}

	pub async fn store_measurements(&self, subject_name: &str, dataset_id: Uuid, measurements: Vec<Measurement>) {
		let key = format!("{subject_name}_{dataset_id}");
		let mut cache = self.measurements.write().await;
		cache.insert(key, measurements);
	}

	pub async fn invalidate_measurements(&self, subject_name: &str, dataset_id: Uuid) {
		let key = format!("{subject_name}_{dataset_id}");
		let mut cache = self.measurements.write().await;
		cache.remove(&key);
	}

	// Clear all cache - optimized to release locks early
	pub async fn clear_all(&self) {
		// Clear each cache independently to minimize lock contention
		self.measurements.write().await.clear();
		self.point_analysis.write().await.clear();
		self.range_analysis.write().await.clear();
	}

	// Get cache statistics
	pub async fn stats(&self) -> CacheStats {
		let measurements = self.measurements.read().await;
		let point_analysis = self.point_analysis.read().await;
		let range_analysis = self.range_analysis.read().await;

		CacheStats { measurement_entries: measurements.len(), point_analysis_entries: point_analysis.len(), range_analysis_entries: range_analysis.len(), total_measurements: measurements.values().map(std::vec::Vec::len).sum(), total_range_points: range_analysis.values().map(std::vec::Vec::len).sum() }
	}
}

#[derive(Debug)]
pub struct CacheStats {
	pub measurement_entries: usize,
	pub point_analysis_entries: usize,
	pub range_analysis_entries: usize,
	pub total_measurements: usize,
	pub total_range_points: usize,
}
