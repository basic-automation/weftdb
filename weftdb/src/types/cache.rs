use std::{
	any::Any, collections::HashMap, fmt::Debug, sync::{Arc, LazyLock}, time::{Duration, Instant}
};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
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

/// A connection holding one transaction. When it was checked out of a database's pool
/// (see `database::conn_pool`), dropping it hands the connection back for the next
/// transaction instead of closing it.
pub struct Connection {
	conn: TursoConnection,
	pool: Option<std::sync::Arc<crate::types::database::conn_pool::Pool>>,
	/// Set once a transaction on this connection failed; it is then closed, not pooled.
	failed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Connection {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Connection").field("pooled", &self.pool.is_some()).finish_non_exhaustive()
	}
}

impl Connection {
	#[must_use]
	pub const fn new(conn: TursoConnection) -> Self {
		Self { conn, pool: None, failed: std::sync::atomic::AtomicBool::new(false) }
	}

	pub(crate) const fn pooled(conn: TursoConnection, pool: Option<std::sync::Arc<crate::types::database::conn_pool::Pool>>) -> Self {
		Self { conn, pool, failed: std::sync::atomic::AtomicBool::new(false) }
	}

	#[must_use]
	pub const fn as_ref(&self) -> &TursoConnection {
		&self.conn
	}

	/// Close this connection when it drops instead of pooling it: a transaction on it
	/// failed, and a fresh connection is the only state nobody has to reason about.
	pub fn discard(&self) {
		self.failed.store(true, std::sync::atomic::Ordering::Release);
	}

	/// The Turso connection itself; it leaves the pool for good.
	#[must_use]
	pub fn into_inner(mut self) -> TursoConnection {
		self.pool = None;
		self.conn.clone()
	}
}

impl std::ops::Deref for Connection {
	type Target = TursoConnection;

	fn deref(&self) -> &TursoConnection {
		&self.conn
	}
}

impl Drop for Connection {
	fn drop(&mut self) {
		if let Some(pool) = self.pool.take().filter(|_| !self.failed.load(std::sync::atomic::Ordering::Acquire)) {
			pool.give_back(self.conn.clone());
		}
	}
}

// Cacheable trait - removed Clone bound to make it dyn-compatible
pub trait Cacheable: Send + Sync + 'static {
	fn as_any(&self) -> &dyn Any;
	fn clone_box(&self) -> Box<dyn Cacheable>;

	/// Roughly how many bytes the value holds, which [`DatabaseCache`] charges against
	/// its byte budget. Small fixed-size values keep the default.
	fn weight(&self) -> usize {
		SMALL_ENTRY_WEIGHT
	}
}

/// What a small value without its own [`Cacheable::weight`] is charged.
const SMALL_ENTRY_WEIGHT: usize = 256;

/// The bytes a list of `len` `T`s holds, before anything its items point to.
const fn list_weight<T>(len: usize) -> usize {
	SMALL_ENTRY_WEIGHT + len * std::mem::size_of::<T>()
}

// Implement Cacheable for your types
impl Cacheable for Vec<Measurement> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}

	fn weight(&self) -> usize {
		list_weight::<Measurement>(self.len())
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

	fn weight(&self) -> usize {
		list_weight::<Batch>(self.len())
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

	fn weight(&self) -> usize {
		list_weight::<Event>(self.len())
	}
}

impl Cacheable for Pattern {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}

	fn weight(&self) -> usize {
		self.approx_heap_bytes()
	}
}

impl Cacheable for Vec<Pattern> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(self.clone())
	}

	fn weight(&self) -> usize {
		SMALL_ENTRY_WEIGHT + self.iter().map(Pattern::approx_heap_bytes).sum::<usize>()
	}
}

impl Cacheable for Arc<[Pattern]> {
	fn as_any(&self) -> &dyn Any {
		self
	}

	fn clone_box(&self) -> Box<dyn Cacheable> {
		Box::new(Self::clone(self))
	}

	fn weight(&self) -> usize {
		SMALL_ENTRY_WEIGHT + self.iter().map(Pattern::approx_heap_bytes).sum::<usize>()
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

	fn weight(&self) -> usize {
		list_weight::<crate::Correlation>(self.len())
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

	fn weight(&self) -> usize {
		list_weight::<crate::DictionaryMetadata>(self.len())
	}
}

// Internal trait object wrapper
struct CacheableEntry {
	data: Box<dyn Cacheable>,
	/// `data.weight()` when it was stored, so removing it gives back what it was charged.
	weight: usize,
	created_at: Instant,
	access_count: u64,
	last_accessed: Instant,
}

impl Debug for CacheableEntry {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CacheableEntry").field("weight", &self.weight).field("created_at", &self.created_at).field("access_count", &self.access_count).field("last_accessed", &self.last_accessed).finish_non_exhaustive()
	}
}

impl CacheableEntry {
	fn new(data: Box<dyn Cacheable>) -> Self {
		let now = Instant::now();
		let weight = data.weight();
		Self { data, weight, created_at: now, access_count: 1, last_accessed: now }
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

/// The entries and the bytes they are charged, kept together under one lock.
#[derive(Debug, Default)]
struct Entries {
	map: HashMap<String, CacheableEntry>,
	bytes: usize,
}

impl Entries {
	fn remove(&mut self, key: &str) {
		if let Some(entry) = self.map.remove(key) {
			self.bytes -= entry.weight;
		}
	}

	fn retain_unexpired(&mut self, ttl: Duration) {
		let mut freed = 0;
		self.map.retain(|_, entry| {
			let keep = !entry.is_expired(ttl);
			if !keep {
				freed += entry.weight;
			}
			keep
		});
		self.bytes -= freed;
	}
}

/// The default byte budget of a [`DatabaseCache`], in MiB; `WEFT_CACHE_MAX_MB` overrides it.
const DEFAULT_CACHE_MAX_MB: usize = 256;

static CACHE_MAX_BYTES: LazyLock<usize> = LazyLock::new(|| std::env::var("WEFT_CACHE_MAX_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(DEFAULT_CACHE_MAX_MB).saturating_mul(1024 * 1024));

/// Values read back from a database, kept for a while so the next read skips the query.
///
/// Bounded three ways: by entry count, by the bytes the entries hold
/// ([`Cacheable::weight`], at most `WEFT_CACHE_MAX_MB`), and by age. Every store first
/// drops expired entries, then the least recently used ones until the new value fits; a
/// value heavier than the whole budget is not cached at all. A dictionary's pattern list
/// with every occurrence log is the heavy case: it is a copy of what is on disk, so
/// leaving it out only costs a reread.
#[derive(Debug)]
pub struct DatabaseCache {
	cache: Arc<RwLock<Entries>>,
	/// How many times each key was invalidated through
	/// [`invalidate_generation`](Self::invalidate_generation); see
	/// [`generation`](Self::generation). Only keys a writer replaces have one, so it stays
	/// small. Always locked after `cache`, never before.
	generations: Arc<RwLock<HashMap<String, u64>>>,
	max_entries: usize,
	max_bytes: usize,
	ttl: Duration,
}

impl DatabaseCache {
	#[must_use]
	pub fn new() -> Self {
		Self::with_max_bytes(*CACHE_MAX_BYTES)
	}

	/// A cache holding at most `max_bytes` of values (by [`Cacheable::weight`]).
	#[must_use]
	pub fn with_max_bytes(max_bytes: usize) -> Self {
		// Memory only: `System::new_all` also walks every process and thread in `/proc`,
		// and a database is opened once per `analyze_range` chunk, so that walk ran for
		// every chunk an analytics pipeline read (#145).
		let sys = System::new_with_specifics(RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()));
		let available_bytes = sys.available_memory();
		let available_gb = available_bytes / (1024 * 1024 * 1024);
		let available_gb = available_gb.max(1) as usize;
		let adjusted_max_entries = available_gb * 100;
		let ttl = Duration::from_secs(600);

		Self { cache: Arc::new(RwLock::new(Entries::default())), generations: Arc::new(RwLock::new(HashMap::new())), max_entries: adjusted_max_entries, max_bytes, ttl }
	}

	/// The bytes the cached values are charged.
	pub async fn bytes(&self) -> usize {
		self.cache.read().await.bytes
	}

	/// Get a value from cache with type checking
	pub async fn get<T: Cacheable + Clone>(&self, cache_key: &str) -> Option<T> {
		let mut cache = self.cache.write().await;

		// Check if entry exists and is expired
		let is_expired = cache.map.get(cache_key).is_some_and(|entry| entry.is_expired(self.ttl));

		if is_expired {
			cache.remove(cache_key);
			return None;
		}

		// Access and return the entry if it exists
		cache.map.get_mut(cache_key).and_then(|entry| {
			let data = entry.access();
			data.as_any().downcast_ref::<T>().cloned()
		})
	}

	/// Store a value in cache
	pub async fn store<T: Cacheable>(&self, cache_key: &str, value: T) {
		let mut cache = self.cache.write().await;
		self.insert(&mut cache, cache_key, Box::new(value));
	}

	/// Put `value` under `cache_key`, making room for it first; see [`DatabaseCache`].
	fn insert(&self, cache: &mut Entries, cache_key: &str, value: Box<dyn Cacheable>) {
		cache.remove(cache_key);
		cache.retain_unexpired(self.ttl);
		let entry = CacheableEntry::new(value);
		if entry.weight > self.max_bytes {
			return;
		}
		if cache.map.len() >= self.max_entries {
			Self::evict_lru(cache);
		}
		if cache.bytes + entry.weight > self.max_bytes {
			let mut by_age: Vec<(Instant, String)> = cache.map.iter().map(|(k, e)| (e.last_accessed, k.clone())).collect();
			by_age.sort_unstable();
			for (_, key) in by_age {
				if cache.bytes + entry.weight <= self.max_bytes {
					break;
				}
				cache.remove(&key);
			}
		}
		cache.bytes += entry.weight;
		cache.map.insert(cache_key.to_string(), entry);
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
		self.cache.write().await.remove(cache_key);
		self.generations.write().await.entry(cache_key.to_string()).and_modify(|generation| *generation = generation.wrapping_add(1)).or_insert(1);
	}

	/// [`store`](Self::store) `value` under `cache_key` unless the key was invalidated with
	/// [`invalidate_generation`](Self::invalidate_generation) since the reader took
	/// `generation`. Returns whether the generation still matched; a value heavier than the
	/// whole byte budget is then still left out, as [`store`](Self::store) leaves it out.
	pub async fn store_if_generation<T: Cacheable>(&self, cache_key: &str, generation: u64, value: T) -> bool {
		let mut cache = self.cache.write().await;
		// Holding `cache` keeps an `invalidate_generation` from running between this check
		// and the insert.
		if self.generations.read().await.get(cache_key).copied().unwrap_or(0) != generation {
			return false;
		}
		self.insert(&mut cache, cache_key, Box::new(value));
		true
	}

	fn evict_lru(cache: &mut Entries) {
		let evict_count = cache.map.len() / 10;
		if evict_count == 0 {
			return;
		}

		let mut entries: Vec<(String, Instant)> = cache.map.iter().map(|(k, entry)| (k.clone(), entry.last_accessed)).collect();
		entries.sort_by_key(|&(_, time)| time);

		for (key, _) in entries.into_iter().take(evict_count) {
			cache.remove(&key);
		}
	}

	pub async fn cleanup_expired(&self) {
		self.cache.write().await.retain_unexpired(self.ttl);
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

	/// A value that says how heavy it is.
	#[derive(Clone)]
	struct Blob(usize);

	impl Cacheable for Blob {
		fn as_any(&self) -> &dyn Any {
			self
		}

		fn clone_box(&self) -> Box<dyn Cacheable> {
			Box::new(self.clone())
		}

		fn weight(&self) -> usize {
			self.0
		}
	}

	/// Values are charged by their weight; storing past the byte budget evicts the least
	/// recently used values, and one heavier than the whole budget is not kept.
	#[tokio::test]
	async fn the_byte_budget_evicts_least_recently_used_values() {
		let cache = DatabaseCache::with_max_bytes(300);
		cache.store("a", Blob(100)).await;
		cache.store("b", Blob(100)).await;
		cache.store("c", Blob(100)).await;
		assert_eq!(cache.bytes().await, 300);
		assert!(cache.get::<Blob>("a").await.is_some(), "touch a so b is the oldest");
		cache.store("d", Blob(100)).await;
		assert!(cache.get::<Blob>("b").await.is_none(), "b was evicted");
		for key in ["a", "c", "d"] {
			assert!(cache.get::<Blob>(key).await.is_some(), "{key} stays");
		}
		assert_eq!(cache.bytes().await, 300);

		cache.store("huge", Blob(301)).await;
		assert!(cache.get::<Blob>("huge").await.is_none(), "a value over the budget is not cached");
		assert_eq!(cache.bytes().await, 300, "nor does it evict anything");

		cache.store("a", Blob(10)).await;
		cache.invalidate("c").await;
		assert_eq!(cache.bytes().await, 110, "replacing and invalidating give back what they were charged");
		cache.store("e", Blob(250)).await;
		assert!(cache.get::<Blob>("e").await.is_some());
		assert!(cache.bytes().await <= 300, "older values made room for e");
	}
}
