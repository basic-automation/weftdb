use anyhow::Result;

use crate::{AspectId, Correlation, CorrelationID, Database};

/// Trait for correlation database operations
#[async_trait::async_trait]
pub trait CorrelationDatabase {
	/// Store correlation in a specific dictionary
	async fn store_correlation_in_dictionary(&self, correlation: &Correlation) -> Result<()>;

	/// Get correlations from a specific dictionary
	async fn get_correlations_from_dictionary(&self, aspect_id: &AspectId) -> Result<Vec<Correlation>>;

	/// Get correlation by ID from a specific dictionary
	async fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<Option<Correlation>>;

	/// Update correlation in a specific dictionary
	async fn update_correlation_in_dictionary(&self, correlation: &Correlation) -> Result<()>;

	/// Delete correlation from a specific dictionary
	async fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<bool>;
}

/// Default implementations for `CorrelationDatabase` with "default" dictionary
#[async_trait::async_trait]
impl CorrelationDatabase for Database {
	async fn store_correlation_in_dictionary(&self, correlation: &Correlation) -> Result<()> {
		// Delegate to the database implementation
		self.store_correlation_in_dictionary(correlation).await
	}

	async fn get_correlations_from_dictionary(&self, aspect_id: &AspectId) -> Result<Vec<Correlation>> {
		self.get_correlations_from_dictionary(aspect_id).await
	}

	async fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<Option<Correlation>> {
		self.get_correlation_by_id_from_dictionary(correlation_id, aspect_id).await
	}

	async fn update_correlation_in_dictionary(&self, correlation: &Correlation) -> Result<()> {
		self.update_correlation_in_dictionary(correlation).await
	}

	async fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<bool> {
		self.delete_correlation_from_dictionary(correlation_id, aspect_id).await
	}
}

/// Extension methods for default dictionary operations
impl Database {
	/// Store a correlation in the default dictionary
	///
	/// # Errors
	/// - if unable to store correlation in dictionary
	pub async fn store_correlation(&self, correlation: &Correlation) -> Result<()> {
		self.store_correlation_in_dictionary(correlation).await
	}

	/// Get all correlations from the default dictionary
	///
	/// # Errors
	/// - if unable to retrieve correlations from dictionary
	pub async fn get_correlations(&self, aspect_id: &AspectId) -> Result<Vec<Correlation>> {
		self.get_correlations_from_dictionary(aspect_id).await
	}

	/// Get correlation by ID from the default dictionary
	///
	/// # Errors
	/// - if unable to retrieve correlation from dictionary
	pub async fn get_correlation_by_id(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<Option<Correlation>> {
		self.get_correlation_by_id_from_dictionary(correlation_id, aspect_id).await
	}

	/// Update correlation in the default dictionary
	///
	/// # Errors
	/// - if unable to update correlation in dictionary
	pub async fn update_correlation(&self, correlation: &Correlation) -> Result<()> {
		self.update_correlation_in_dictionary(correlation).await
	}

	/// Delete correlation from the default dictionary
	///
	/// # Errors
	/// - if unable to delete correlation from dictionary
	pub async fn delete_correlation(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<bool> {
		self.delete_correlation_from_dictionary(correlation_id, aspect_id).await
	}
}
