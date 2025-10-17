use std::{
	collections::HashMap, sync::Arc, time::{Duration, Instant}
};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{Batch, Measurement};

#[derive(Debug, Clone)]
pub struct AnalysisResult {
	timestamp: DateTime<Utc>,
	value: BigDecimal,
	method: String,
	resolution: String,
}

impl AnalysisResult {
	#[must_use]
	pub const fn new(timestamp: DateTime<Utc>, value: BigDecimal, method: String, resolution: String) -> Self {
		Self { timestamp, value, method, resolution }
	}

	#[must_use]
	pub const fn timestamp(&self) -> DateTime<Utc> {
		self.timestamp
	}

	#[must_use]
	pub const fn value(&self) -> &BigDecimal {
		&self.value
	}

	#[must_use]
	pub fn method(&self) -> &str {
		&self.method
	}

	#[must_use]
	pub fn resolution(&self) -> &str {
		&self.resolution
	}
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
	unprocessed_batch_cache: Arc<RwLock<HashMap<String, CacheEntry<Vec<Batch>>>>>,

	// Cache configuration
	max_measurement_entries: usize,
	max_analysis_entries: usize,
	max_unprocessed_batch_entries: usize,

	measurement_ttl: Duration,
	analysis_ttl: Duration,
	unprocessed_batch_ttl: Duration,
}

impl DatabaseCache {
	pub async fn get_measurements(&self, cache_key: &str) -> Option<Vec<Measurement>> {
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

	pub async fn store_measurements(&self, cache_key: &str, measurements: &[Measurement]) {
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

	// NEW: Unprocessed batches cache methods
	pub async fn get_unprocessed_batches(&self, cache_key: &str) -> Option<Vec<Batch>> {
		let mut cache = self.unprocessed_batch_cache.write().await;

		if let Some(entry) = cache.get_mut(cache_key) {
			if entry.is_expired(self.unprocessed_batch_ttl) {
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

	pub async fn store_unprocessed_batches(&self, cache_key: &str, batches: &[Batch]) {
		let mut cache = self.unprocessed_batch_cache.write().await;

		// Implement LRU eviction if cache is full
		if cache.len() >= self.max_unprocessed_batch_entries {
			Self::evict_lru_unprocessed_batches(&mut cache);
		}

		cache.insert(cache_key.to_string(), CacheEntry::new(batches.to_vec()));
	}

	pub async fn get_aspect_measurements(&self, cache_key: &str) -> Option<Vec<Measurement>> {
		// This is an alias for get_measurements with aspect-specific logic
		self.get_measurements(cache_key).await
	}

	pub async fn store_aspect_measurements(&self, cache_key: &str, measurements: &[Measurement]) {
		// This is an alias for store_measurements with aspect-specific logic
		self.store_measurements(cache_key, measurements).await;
	}

	pub async fn invalidate_aspect_cache(&self, cache_key: &str) {
		// Remove from measurements cache
		self.measurement_cache.write().await.remove(cache_key);

		// Remove from unprocessed batches cache
		self.unprocessed_batch_cache.write().await.remove(cache_key);

		// Extract aspect ID from cache key to remove related analysis cache entries
		// Cache keys typically follow patterns like: "aspect_measurements_{aspect_id}_{size}" or "unprocessed_batch_{aspect_id}_{batch_id}"
		if let Some(aspect_id_str) = Self::extract_aspect_id_from_cache_key(cache_key) {
			let mut analysis_cache = self.point_analysis_cache.write().await;
			analysis_cache.retain(|key, _| !key.contains(&aspect_id_str));
		}
	}

	// Helper function to extract aspect ID from cache key
	fn extract_aspect_id_from_cache_key(cache_key: &str) -> Option<String> {
		// Handle different cache key patterns:
		// - "aspect_measurements_{aspect_id}_{size}"
		// - "unprocessed_batch_{aspect_id}_{batch_id}"
		// - "point_{aspect_id}_{timestamp}_{nanos}_{resolution}_{method}"
		// - etc.

		if cache_key.starts_with("aspect_measurements_") {
			// Extract from "aspect_measurements_{aspect_id}_{size}"
			let parts: Vec<&str> = cache_key.split('_').collect();
			if parts.len() >= 3 {
				return Some(parts[2].to_string());
			}
		} else if cache_key.starts_with("unprocessed_batch_") {
			// Extract from "unprocessed_batch_{aspect_id}_{batch_id}"
			let parts: Vec<&str> = cache_key.split('_').collect();
			if parts.len() >= 3 {
				return Some(parts[2].to_string());
			}
		} else if cache_key.starts_with("point_") {
			// Extract from "point_{aspect_id}_{timestamp}_{nanos}_{resolution}_{method}"
			let parts: Vec<&str> = cache_key.split('_').collect();
			if parts.len() >= 2 {
				return Some(parts[1].to_string());
			}
		}

		// Fallback: try to find any UUID-like pattern in the cache key
		// UUIDs are 36 characters with hyphens at positions 8, 13, 18, 23
		for part in cache_key.split('_') {
			if part.len() == 36 && part.chars().filter(|&c| c == '-').count() == 4 {
				// Validate it looks like a UUID
				if part.chars().nth(8) == Some('-') && part.chars().nth(13) == Some('-') && part.chars().nth(18) == Some('-') && part.chars().nth(23) == Some('-') {
					return Some(part.to_string());
				}
			}
		}

		None
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

	// NEW: LRU eviction for unprocessed batches
	fn evict_lru_unprocessed_batches(cache: &mut HashMap<String, CacheEntry<Vec<Batch>>>) {
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
			unprocessed_batch_cache: Arc::new(RwLock::new(HashMap::new())),
			max_measurement_entries: 1000,
			max_analysis_entries: 10000,
			max_unprocessed_batch_entries: 100,
			measurement_ttl: Duration::from_secs(300),       // 5 minutes
			analysis_ttl: Duration::from_secs(60),           // 1 minute
			unprocessed_batch_ttl: Duration::from_secs(600), // 10 minutes
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

		// NEW: Cleanup expired unprocessed batches
		{
			let mut unprocessed_batch_cache = self.unprocessed_batch_cache.write().await;
			unprocessed_batch_cache.retain(|_, entry| !entry.is_expired(self.unprocessed_batch_ttl));
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
