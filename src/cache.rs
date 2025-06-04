use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{Dataset, Measurement};

#[derive(Debug, Clone)]
pub struct CacheEntry {
	pub dataset: Dataset,
	pub last_accessed: std::time::Instant,
}

#[derive(Debug)]
pub struct DatabaseCache {
	// subject_name -> dataset_id -> CacheEntry
	datasets: Arc<RwLock<HashMap<String, HashMap<Uuid, CacheEntry>>>>,
	// subject_name -> dataset_name -> dataset_id (for name lookups)
	dataset_names: Arc<RwLock<HashMap<String, HashMap<String, Uuid>>>>,
	max_entries_per_subject: usize,
	ttl_seconds: u64,
}

impl DatabaseCache {
	pub fn new(max_entries_per_subject: usize, ttl_seconds: u64) -> Self {
		Self { datasets: Arc::new(RwLock::new(HashMap::new())), dataset_names: Arc::new(RwLock::new(HashMap::new())), max_entries_per_subject, ttl_seconds }
	}

	/// Cache a dataset
	pub async fn cache_dataset(&self, subject_name: &str, dataset: Dataset) {
		let mut datasets = self.datasets.write().await;
		let mut dataset_names = self.dataset_names.write().await;

		let subject_datasets = datasets.entry(subject_name.to_string()).or_insert_with(HashMap::new);
		let subject_names = dataset_names.entry(subject_name.to_string()).or_insert_with(HashMap::new);

		// Check if we need to evict old entries
		if subject_datasets.len() >= self.max_entries_per_subject {
			self.evict_oldest_entry(subject_datasets, subject_names).await;
		}

		let cache_entry = CacheEntry { dataset: dataset.clone(), last_accessed: std::time::Instant::now() };

		subject_datasets.insert(dataset.id, cache_entry);
		subject_names.insert(dataset.name.clone(), dataset.id);

		println!("DEBUG: Cached dataset {} for subject {}", dataset.name, subject_name);
	}

	/// Get a dataset by ID
	pub async fn get_dataset(&self, subject_name: &str, dataset_id: Uuid) -> Option<Dataset> {
		let mut datasets = self.datasets.write().await;

		if let Some(subject_datasets) = datasets.get_mut(subject_name) {
			if let Some(entry) = subject_datasets.get_mut(&dataset_id) {
				// Check TTL
				if entry.last_accessed.elapsed().as_secs() > self.ttl_seconds {
					subject_datasets.remove(&dataset_id);
					// Also remove from names cache
					let mut dataset_names = self.dataset_names.write().await;
					if let Some(subject_names) = dataset_names.get_mut(subject_name) {
						subject_names.retain(|_, &mut id| id != dataset_id);
					}
					println!("DEBUG: Cache entry expired for dataset {} in subject {}", dataset_id, subject_name);
					return None;
				}

				// Update last accessed time
				entry.last_accessed = std::time::Instant::now();
				println!("DEBUG: Cache hit for dataset {} in subject {}", dataset_id, subject_name);
				return Some(entry.dataset.clone());
			}
		}

		println!("DEBUG: Cache miss for dataset {} in subject {}", dataset_id, subject_name);
		None
	}

	/// Get a dataset by name
	pub async fn get_dataset_by_name(&self, subject_name: &str, dataset_name: &str) -> Option<Dataset> {
		let dataset_names = self.dataset_names.read().await;

		if let Some(subject_names) = dataset_names.get(subject_name) {
			if let Some(&dataset_id) = subject_names.get(dataset_name) {
				drop(dataset_names); // Release the read lock
				return self.get_dataset(subject_name, dataset_id).await;
			}
		}

		println!("DEBUG: Cache miss for dataset name '{}' in subject {}", dataset_name, subject_name);
		None
	}

	/// Get dataset ID by name
	pub async fn get_dataset_id_by_name(&self, subject_name: &str, dataset_name: &str) -> Option<Uuid> {
		let dataset_names = self.dataset_names.read().await;

		if let Some(subject_names) = dataset_names.get(subject_name) {
			if let Some(&dataset_id) = subject_names.get(dataset_name) {
				println!("DEBUG: Cache hit for dataset name '{}' -> {} in subject {}", dataset_name, dataset_id, subject_name);
				return Some(dataset_id);
			}
		}

		println!("DEBUG: Cache miss for dataset name '{}' in subject {}", dataset_name, subject_name);
		None
	}

	/// Add a measurement to a cached dataset
	pub async fn add_measurement_to_cache(&self, subject_name: &str, dataset_id: Uuid, measurement: Measurement) -> bool {
		let mut datasets = self.datasets.write().await;

		if let Some(subject_datasets) = datasets.get_mut(subject_name) {
			if let Some(entry) = subject_datasets.get_mut(&dataset_id) {
				entry.dataset.measurements.push(measurement);
				entry.last_accessed = std::time::Instant::now();
				println!("DEBUG: Added measurement to cached dataset {} in subject {}", dataset_id, subject_name);
				return true;
			}
		}

		false
	}

	/// Get measurements for a dataset
	pub async fn get_measurements(&self, subject_name: &str, dataset_id: Uuid) -> Option<Vec<Measurement>> {
		if let Some(dataset) = self.get_dataset(subject_name, dataset_id).await {
			println!("DEBUG: Retrieved {} measurements from cache for dataset {} in subject {}", dataset.measurements.len(), dataset_id, subject_name);
			return Some(dataset.measurements);
		}
		None
	}

	/// Clear cache for a specific subject
	pub async fn clear_subject_cache(&self, subject_name: &str) {
		let mut datasets = self.datasets.write().await;
		let mut dataset_names = self.dataset_names.write().await;

		datasets.remove(subject_name);
		dataset_names.remove(subject_name);

		println!("DEBUG: Cleared cache for subject {}", subject_name);
	}

	/// Clear all cache
	pub async fn clear_all(&self) {
		let mut datasets = self.datasets.write().await;
		let mut dataset_names = self.dataset_names.write().await;

		datasets.clear();
		dataset_names.clear();

		println!("DEBUG: Cleared all cache");
	}

	/// Get cache statistics
	pub async fn get_stats(&self) -> HashMap<String, usize> {
		let datasets = self.datasets.read().await;
		let mut stats = HashMap::new();

		for (subject_name, subject_datasets) in datasets.iter() {
			stats.insert(subject_name.clone(), subject_datasets.len());
		}

		stats
	}

	/// Evict the oldest entry from a subject's cache
	async fn evict_oldest_entry(&self, subject_datasets: &mut HashMap<Uuid, CacheEntry>, subject_names: &mut HashMap<String, Uuid>) {
		if let Some((&oldest_id, _)) = subject_datasets.iter().min_by_key(|(_, entry)| entry.last_accessed) {
			if let Some(removed_entry) = subject_datasets.remove(&oldest_id) {
				// Remove from names cache as well
				subject_names.retain(|_, &mut id| id != oldest_id);
				println!("DEBUG: Evicted oldest dataset {} from cache", removed_entry.dataset.name);
			}
		}
	}

	/// Clean up expired entries
	pub async fn cleanup_expired(&self) {
		let mut datasets = self.datasets.write().await;
		let mut dataset_names = self.dataset_names.write().await;

		let now = std::time::Instant::now();
		let mut expired_datasets = Vec::new();

		for (subject_name, subject_datasets) in datasets.iter() {
			for (&dataset_id, entry) in subject_datasets.iter() {
				if now.duration_since(entry.last_accessed).as_secs() > self.ttl_seconds {
					expired_datasets.push((subject_name.clone(), dataset_id, entry.dataset.name.clone()));
				}
			}
		}

		for (subject_name, dataset_id, dataset_name) in expired_datasets {
			if let Some(subject_datasets) = datasets.get_mut(&subject_name) {
				subject_datasets.remove(&dataset_id);
			}
			if let Some(subject_names) = dataset_names.get_mut(&subject_name) {
				subject_names.remove(&dataset_name);
			}
			println!("DEBUG: Cleaned up expired dataset {} from subject {}", dataset_name, subject_name);
		}
	}
}

impl Default for DatabaseCache {
	fn default() -> Self {
		Self::new(100, 3600) // 100 datasets per subject, 1 hour TTL
	}
}
