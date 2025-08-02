use std::{
	collections::HashMap, sync::Arc, time::{Duration, Instant}
};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::Measurement;

#[derive(Debug, Clone)]
pub struct AnalysisResult {
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
	pub method: String,
	pub resolution: String,
}

#[derive(Debug, Clone)]
struct CacheEntry<T> {
	data: T,
	created_at: Instant,
	access_count: u64,
	last_accessed: Instant,
}

impl<T> CacheEntry<T> {
	fn new(data: T) -> Self {
		let now = Instant::now();
		Self { data, created_at: now, access_count: 1, last_accessed: now }
	}

	fn access(&mut self) -> &T {
		self.access_count += 1;
		self.last_accessed = Instant::now();
		&self.data
	}

	fn is_expired(&self, max_age: Duration) -> bool {
		self.created_at.elapsed() > max_age
	}
}

#[derive(Debug)]
pub struct DatabaseCache {
	// Separate caches with different eviction policies
	measurement_cache: Arc<RwLock<HashMap<String, CacheEntry<Vec<Measurement>>>>>,
	point_analysis_cache: Arc<RwLock<HashMap<String, CacheEntry<AnalysisResult>>>>,

	// Cache configuration
	max_measurement_entries: usize,
	max_analysis_entries: usize,
	measurement_ttl: Duration,
	analysis_ttl: Duration,
}

impl DatabaseCache {
	pub async fn get_aspect_measurements(&self, cache_key: &str, _dataset_id: Uuid) -> Option<Vec<Measurement>> {
		let mut cache = self.measurement_cache.write().await;

		if let Some(entry) = cache.get_mut(cache_key) {
			if entry.is_expired(self.measurement_ttl) {
				cache.remove(cache_key);
			} else {
				let result = entry.access().clone();
				drop(cache);
				return Some(result);
			}
		}
		drop(cache);
		None
	}

	pub async fn store_aspect_measurements(&self, cache_key: &str, measurements: &[Measurement], _dataset_id: Uuid) {
		let mut cache = self.measurement_cache.write().await;

		// Implement LRU eviction if cache is full
		if cache.len() >= self.max_measurement_entries {
			Self::evict_lru_measurements(&mut cache);
		}

		cache.insert(cache_key.to_string(), CacheEntry::new(measurements.to_vec()));
	}

	pub async fn get_point_analysis(&self, cache_key: &str) -> Option<AnalysisResult> {
		let mut cache = self.point_analysis_cache.write().await;

		if let Some(entry) = cache.get_mut(cache_key) {
			if entry.is_expired(self.analysis_ttl) {
				cache.remove(cache_key);
			} else {
				let result = entry.access().clone();
				drop(cache);
				return Some(result);
			}
		}
		drop(cache);
		None
	}

	pub async fn store_point_analysis(&self, cache_key: &str, result: &AnalysisResult) {
		let mut cache = self.point_analysis_cache.write().await;

		// Implement LRU eviction if cache is full
		if cache.len() >= self.max_analysis_entries {
			Self::evict_lru_analysis(&mut cache);
		}

		cache.insert(cache_key.to_string(), CacheEntry::new(result.clone()));
	}

	pub async fn invalidate_aspect_cache(&self, cache_key: &str, _dataset_id: Uuid) {
		// Remove from measurements cache
		self.measurement_cache.write().await.remove(cache_key);

		// Remove related analysis cache entries (they contain the aspect ID)
		let aspect_id_str = cache_key.split('_').next_back().unwrap_or("");
		let mut analysis_cache = self.point_analysis_cache.write().await;
		analysis_cache.retain(|key, _| !key.contains(aspect_id_str));
	}

	fn evict_lru_measurements(cache: &mut HashMap<String, CacheEntry<Vec<Measurement>>>) {
		// Remove 10% of entries, prioritizing least recently used
		let evict_count = cache.len() / 10;
		if evict_count == 0 {
			return;
		}

		let mut entries: Vec<(String, Instant)> = cache.iter().map(|(k, entry)| (k.clone(), entry.last_accessed)).collect();
		entries.sort_by_key(|&(_, time)| time);

		for (key, _) in entries.into_iter().take(evict_count) {
			cache.remove(&key);
		}
	}

	fn evict_lru_analysis(cache: &mut HashMap<String, CacheEntry<AnalysisResult>>) {
		// Remove 10% of entries, prioritizing least recently used
		let evict_count = cache.len() / 10;
		if evict_count == 0 {
			return;
		}

		let mut entries: Vec<(String, Instant)> = cache.iter().map(|(k, entry)| (k.clone(), entry.last_accessed)).collect();
		entries.sort_by_key(|&(_, time)| time);

		for (key, _) in entries.into_iter().take(evict_count) {
			cache.remove(&key);
		}
	}

	#[must_use]
	pub fn new() -> Self {
		Self {
			measurement_cache: Arc::new(RwLock::new(HashMap::new())),
			point_analysis_cache: Arc::new(RwLock::new(HashMap::new())),
			max_measurement_entries: 1000,
			max_analysis_entries: 10000,
			measurement_ttl: Duration::from_secs(300), // 5 minutes
			analysis_ttl: Duration::from_secs(60),     // 1 minute
		}
	}

	// Periodic cleanup method to remove expired entries
	pub async fn cleanup_expired(&self) {
		{
			let mut measurement_cache = self.measurement_cache.write().await;
			measurement_cache.retain(|_, entry| !entry.is_expired(self.measurement_ttl));
		}

		{
			let mut analysis_cache = self.point_analysis_cache.write().await;
			analysis_cache.retain(|_, entry| !entry.is_expired(self.analysis_ttl));
		}
	}
}

impl Default for DatabaseCache {
	fn default() -> Self {
		Self::new()
	}
}

// Global cache instance
use std::sync::LazyLock;
pub static CACHE: LazyLock<DatabaseCache> = LazyLock::new(DatabaseCache::new);
