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
	#[must_use]
	pub fn new(max_entries_per_subject: usize, ttl_seconds: u64) -> Self {
		Self { datasets: Arc::new(RwLock::new(HashMap::new())), dataset_names: Arc::new(RwLock::new(HashMap::new())), max_entries_per_subject, ttl_seconds }
	}

	/// Cache a dataset
	#[allow(clippy::significant_drop_tightening)]
	pub async fn cache_dataset(&self, subject_name: &str, dataset: Dataset) {
		let subject_name_string = subject_name.to_string();
		let dataset_name = dataset.name.clone();
		let dataset_id = dataset.id;

		let cache_entry = CacheEntry { dataset: dataset.clone(), last_accessed: std::time::Instant::now() };

		// Handle datasets cache with minimal lock time
		{
			let mut datasets = self.datasets.write().await;
			let subject_datasets = datasets.entry(subject_name_string.clone()).or_insert_with(HashMap::new);

			// Check if we need to evict entries due to size limit
			if subject_datasets.len() >= self.max_entries_per_subject {
				// Find and remove the oldest entry
				if let Some((oldest_id, ())) = subject_datasets.iter().min_by_key(|(_, entry)| entry.last_accessed).map(|(id, _)| (*id, ())) {
					subject_datasets.remove(&oldest_id);
				}
			}

			subject_datasets.insert(dataset_id, cache_entry);
		}

		// Handle dataset names cache with minimal lock time
		{
			// First, check what needs to be cleaned up
			let cleanup_needed = {
				let datasets_read = self.datasets.read().await;
				let subject_datasets = datasets_read.get(&subject_name_string);
				subject_datasets.map(|sd| {
					// Collect IDs that should be retained
					sd.keys().copied().collect::<std::collections::HashSet<_>>()
				})
			};

			// Now acquire write lock and do the cleanup + insert
			let mut dataset_names = self.dataset_names.write().await;
			let subject_names = dataset_names.entry(subject_name_string).or_insert_with(HashMap::new);

			if let Some(valid_ids) = cleanup_needed {
				subject_names.retain(|_, &mut id| valid_ids.contains(&id));
			}

			subject_names.insert(dataset_name, dataset_id);
		}

		println!("DEBUG: Cached dataset {} for subject {}", dataset.name, subject_name);
	}

	/// Get a dataset by ID
	pub async fn get_dataset(&self, subject_name: &str, dataset_id: Uuid) -> Option<Dataset> {
		let mut datasets = self.datasets.write().await;

		if let Some(subject_datasets) = datasets.get_mut(subject_name)
			&& let Some(entry) = subject_datasets.get_mut(&dataset_id)
		{
			// Check TTL
			if entry.last_accessed.elapsed().as_secs() > self.ttl_seconds {
				subject_datasets.remove(&dataset_id);
				drop(datasets); // Release the lock before acquiring another

				// Also remove from names cache
				if let Some(subject_names) = self.dataset_names.write().await.get_mut(subject_name) {
					subject_names.retain(|_, &mut id| id != dataset_id);
				}
				return None;
			}

			// Update access time and return clone
			entry.last_accessed = std::time::Instant::now();
			return Some(entry.dataset.clone());
		}
		None
	}

	/// Get a dataset by name
	pub async fn get_dataset_by_name(&self, subject_name: &str, dataset_name: &str) -> Option<Dataset> {
		let dataset_names = self.dataset_names.read().await;

		if let Some(subject_names) = dataset_names.get(subject_name)
			&& let Some(&dataset_id) = subject_names.get(dataset_name)
		{
			drop(dataset_names); // Release the read lock
			return self.get_dataset(subject_name, dataset_id).await;
		}

		println!("DEBUG: Cache miss for dataset name '{dataset_name}' in subject {subject_name}");
		None
	}

	/// Get dataset ID by name
	pub async fn get_dataset_id_by_name(&self, subject_name: &str, dataset_name: &str) -> Option<Uuid> {
		let dataset_names = self.dataset_names.read().await;
		dataset_names.get(subject_name)?.get(dataset_name).copied()
	}

	/// Add a measurement to a cached dataset
	pub async fn add_measurement_to_cache(&self, subject_name: &str, dataset_id: Uuid, measurement: Measurement) -> bool {
		if let Some(subject_datasets) = self.datasets.write().await.get_mut(subject_name)
			&& let Some(entry) = subject_datasets.get_mut(&dataset_id)
		{
			entry.dataset.measurements.push(measurement);
			entry.last_accessed = std::time::Instant::now();
			return true;
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

	/// Get measurements for a dataset by time range
	pub async fn get_measurements_by_time(&self, subject_name: &str, dataset_id: Uuid, start_time: chrono::DateTime<chrono::Utc>, end_time: chrono::DateTime<chrono::Utc>) -> Option<Vec<Measurement>> {
		if let Some(dataset) = self.get_dataset(subject_name, dataset_id).await {
			// Filter measurements by time range
			let filtered_measurements: Vec<Measurement> = dataset.measurements.iter().filter(|m| m.timestamp >= start_time && m.timestamp <= end_time).cloned().collect();

			println!("DEBUG: Retrieved {} measurements from cache for dataset {} in subject {} (time range)", filtered_measurements.len(), dataset_id, subject_name);

			return Some(filtered_measurements);
		}

		println!("DEBUG: Cache miss for dataset {dataset_id} in subject {subject_name} (time range)");
		None
	}

	/// Clear cache for a specific subject
	pub async fn clear_subject_cache(&self, subject_name: &str) {
		self.datasets.write().await.remove(subject_name);
		self.dataset_names.write().await.remove(subject_name);

		println!("DEBUG: Cleared cache for subject {subject_name}");
	}

	/// Clear all cache
	pub async fn clear_all(&self) {
		self.datasets.write().await.clear();
		self.dataset_names.write().await.clear();

		println!("DEBUG: Cleared all cache");
	}

	/// Get cache statistics
	pub async fn get_stats(&self) -> HashMap<String, usize> {
		let mut stats = HashMap::new();

		for (subject_name, subject_datasets) in self.datasets.read().await.iter() {
			stats.insert(subject_name.clone(), subject_datasets.len());
		}

		stats
	}

	/// Clean up expired entries
	pub async fn cleanup_expired(&self) {
		let mut datasets = self.datasets.write().await;
		let expired_threshold = self.ttl_seconds;

		let mut subjects_to_clean = Vec::new();

		for (subject_name, subject_datasets) in datasets.iter_mut() {
			let mut expired_ids = Vec::new();

			for (dataset_id, entry) in subject_datasets.iter() {
				if entry.last_accessed.elapsed().as_secs() > expired_threshold {
					expired_ids.push(*dataset_id);
				}
			}

			for expired_id in expired_ids {
				subject_datasets.remove(&expired_id);
			}

			if !subject_datasets.is_empty() {
				subjects_to_clean.push(subject_name.clone());
			}
		}

		drop(datasets);

		// Clean up name mappings for subjects that had expired entries
		let mut dataset_names = self.dataset_names.write().await;
		for subject_name in subjects_to_clean {
			if let Some(subject_names) = dataset_names.get_mut(&subject_name) {
				// Remove entries that no longer exist in the main cache
				let datasets_read = self.datasets.read().await;
				if let Some(subject_datasets) = datasets_read.get(&subject_name) {
					subject_names.retain(|_, &mut id| subject_datasets.contains_key(&id));
				}
			}
		}
	}

	/// Evict the oldest entry from a subject's cache
	#[allow(dead_code)]
	fn evict_oldest_entry(subject_datasets: &mut HashMap<Uuid, CacheEntry>, subject_names: &mut HashMap<String, Uuid>) {
		if let Some((oldest_id, oldest_name)) = subject_datasets.iter().min_by_key(|(_, entry)| entry.last_accessed).map(|(id, entry)| (*id, entry.dataset.name.clone())) {
			subject_datasets.remove(&oldest_id);
			subject_names.remove(&oldest_name);
		}
	}
}

impl Default for DatabaseCache {
	fn default() -> Self {
		Self::new(100, 3600) // 100 datasets per subject, 1 hour TTL
	}
}
