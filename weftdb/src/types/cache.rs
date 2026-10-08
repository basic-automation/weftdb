use std::{
	any::Any, collections::HashMap, fmt::Debug, sync::Arc, time::{Duration, Instant}
};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use sysinfo::System;
use tokio::sync::RwLock;
use turso::Connection as TursoConnection;

use crate::{Batch, Event, Measurement, Pattern};

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
pub struct Connection(pub TursoConnection);

impl Connection {
	#[must_use]
	pub const fn new(conn: TursoConnection) -> Self {
		Self(conn)
	}

	#[must_use]
	pub const fn as_ref(&self) -> &TursoConnection {
		&self.0
	}

	#[must_use]
	pub fn into_inner(self) -> TursoConnection {
		self.0
	}
}

// Cacheable trait - removed Clone bound to make it dyn-compatible
pub trait Cacheable: Send + Sync + 'static {
	fn as_any(&self) -> &dyn Any;
	fn clone_box(&self) -> Box<dyn Cacheable>;
}

// Implement Cacheable for your types
impl Cacheable for Vec<Measurement> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for AnalysisResult {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Vec<Batch> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Connection {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for DateTime<Utc> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(*self)
	}
}

impl Cacheable for turso::Database {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for String {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for crate::Aspect {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Event {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Vec<Event> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Pattern {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Vec<Pattern> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for crate::Correlation {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Vec<crate::Correlation> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for crate::DictionaryMetadata {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

impl Cacheable for Vec<crate::DictionaryMetadata> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}
}

// Internal trait object wrapper
struct CacheableEntry {
	data: Box<dyn Cacheable>,
	created_at: Instant,
	access_count: u64,
	last_accessed: Instant,
}

impl Debug for CacheableEntry {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CacheableEntry").field("created_at", &self.created_at).field("access_count", &self.access_count).field("last_accessed", &self.last_accessed).finish_non_exhaustive()
	}
}

impl CacheableEntry {
	fn new(data: Box<dyn Cacheable>) -> Self {
		let now = Instant::now();
		Self { data, created_at: now, access_count: 1, last_accessed: now }
	}

	fn access(&mut self) -> &dyn Cacheable {
		self.access_count += 1;
		self.last_accessed = Instant::now();
		self.data.as_ref()
	}

	fn is_expired(&self, max_age: Duration) -> bool {
		self.created_at.elapsed() > max_age
	}
}

#[derive(Debug)]
pub struct DatabaseCache {
	cache: Arc<RwLock<HashMap<String, CacheableEntry>>>,
	/// How many times each key was invalidated through
	/// [`invalidate_generation`](Self::invalidate_generation); see
	/// [`generation`](Self::generation). Only keys a writer replaces have one, so it stays
	/// small. Always locked after `cache`, never before.
	generations: Arc<RwLock<HashMap<String, u64>>>,
	max_entries: usize,
	ttl: Duration,
}

impl DatabaseCache {
	#[must_use]
	pub fn new() -> Self {
		let mut sys = System::new_all();
		sys.refresh_memory();
		let available_bytes = sys.available_memory();
		let available_gb = available_bytes / (1024 * 1024 * 1024);
		let available_gb = available_gb.max(1) as usize;
		let adjusted_max_entries = available_gb * 100;
		let ttl = Duration::from_secs(600);

		Self { cache: Arc::new(RwLock::new(HashMap::new())), generations: Arc::new(RwLock::new(HashMap::new())), max_entries: adjusted_max_entries, ttl }
	}

	/// Get a value from cache with type checking
	pub async fn get<T: Cacheable + Clone>(&self, cache_key: &str) -> Option<T> {
		let mut cache = self.cache.write().await;

		// Check if entry exists and is expired
		let is_expired = cache.get(cache_key).is_some_and(|entry| entry.is_expired(self.ttl));

		if is_expired {
			cache.remove(cache_key);
			return None;
		}

		// Access and return the entry if it exists
		cache.get_mut(cache_key).and_then(|entry| {
			let data = entry.access();
			data.as_any().downcast_ref::<T>().cloned()
		})
	}

	/// Store a value in cache
	pub async fn store<T: Cacheable>(&self, cache_key: &str, value: T) {
		let mut cache = self.cache.write().await;

		if cache.len() >= self.max_entries {
			Self::evict_lru(&mut cache);
		}

		cache.insert(cache_key.to_string(), CacheableEntry::new(Box::new(value)));
	}

	pub async fn invalidate(&self, cache_key: &str) {
		let mut cache = self.cache.write().await;
		cache.remove(cache_key);
	}

	/// The generation of `cache_key`: how many times a writer has invalidated it with
	/// [`invalidate_generation`](Self::invalidate_generation).
	///
	/// A reader takes it *before* it opens the transaction it reads the value in, and
	/// caches what it read with [`store_if_generation`](Self::store_if_generation). A
	/// writer that commits a new value in between invalidates the key after its commit,
	/// which a plain [`store`](Self::store) by that reader would undo: its snapshot can
	/// predate the commit, and the old value would then be served until the TTL expires.
	pub async fn generation(&self, cache_key: &str) -> u64 {
		self.generations.read().await.get(cache_key).copied().unwrap_or(0)
	}

	/// Remove `cache_key`'s value and start its next [`generation`](Self::generation), so
	/// a reader that took the previous one does not store what it read. A writer calls it
	/// after its commit.
	pub async fn invalidate_generation(&self, cache_key: &str) {
		let mut cache = self.cache.write().await;
		cache.remove(cache_key);
		let mut generations = self.generations.write().await;
		let generation = generations.entry(cache_key.to_string()).or_insert(0);
		*generation = generation.wrapping_add(1);
	}

	/// [`store`](Self::store) `value` under `cache_key` unless the key was invalidated with
	/// [`invalidate_generation`](Self::invalidate_generation) since the reader took
	/// `generation`. Returns whether it stored the value.
	pub async fn store_if_generation<T: Cacheable>(&self, cache_key: &str, generation: u64, value: T) -> bool {
		let mut cache = self.cache.write().await;
		// Holding `cache` keeps an `invalidate_generation` from running between this check
		// and the insert.
		if self.generations.read().await.get(cache_key).copied().unwrap_or(0) != generation {
			return false;
		}
		if cache.len() >= self.max_entries {
			Self::evict_lru(&mut cache);
		}
		cache.insert(cache_key.to_string(), CacheableEntry::new(Box::new(value)));
		true
	}

	fn evict_lru(cache: &mut HashMap<String, CacheableEntry>) {
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

	pub async fn cleanup_expired(&self) {
		let mut cache = self.cache.write().await;
		cache.retain(|_, entry| !entry.is_expired(self.ttl));
	}
}

impl Default for DatabaseCache {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A reader that took the generation before a writer's commit does not cache what it
	/// read: its snapshot may predate the commit.
	#[tokio::test]
	async fn a_store_from_before_an_invalidation_is_skipped() {
		let cache = DatabaseCache::new();
		let reader = cache.generation("key").await;
		cache.store("key", "old".to_string()).await;
		cache.invalidate_generation("key").await;
		assert_eq!(cache.get::<String>("key").await, None, "the writer removed the value");

		assert!(!cache.store_if_generation("key", reader, "old".to_string()).await, "the reader's store is skipped");
		assert_eq!(cache.get::<String>("key").await, None);

		let next = cache.generation("key").await;
		assert_ne!(next, reader);
		assert!(cache.store_if_generation("key", next, "new".to_string()).await, "a reader after the write stores");
		assert_eq!(cache.get::<String>("key").await.as_deref(), Some("new"));
		assert!(cache.store_if_generation("other", cache.generation("other").await, "x".to_string()).await, "a key never invalidated is at generation 0");
	}
}
