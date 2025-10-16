use anyhow::Result;

/// Re-export the trait from types/database
pub use crate::types::database::correlation_database::CorrelationDatabase;
use crate::{Correlation, CorrelationID, Database};

/// Default implementations for `CorrelationDatabase` with "default" dictionary
#[async_trait::async_trait]
impl CorrelationDatabase for Database {
	async fn store_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> Result<()> {
		// Delegate to the database implementation
		self.store_correlation_in_dictionary(correlation, dictionary_name).await
	}

	async fn get_correlations_from_dictionary(&self, dictionary_name: &str) -> Result<Vec<Correlation>> {
		self.get_correlations_from_dictionary(dictionary_name).await
	}

	async fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> Result<Option<Correlation>> {
		self.get_correlation_by_id_from_dictionary(correlation_id, dictionary_name).await
	}

	async fn update_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> Result<()> {
		self.update_correlation_in_dictionary(correlation, dictionary_name).await
	}

	async fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> Result<bool> {
		self.delete_correlation_from_dictionary(correlation_id, dictionary_name).await
	}
}

/// Extension methods for default dictionary operations
impl Database {
	/// Store a correlation in the default dictionary
	pub async fn store_correlation(&self, correlation: &Correlation) -> Result<()> {
		self.store_correlation_in_dictionary(correlation, "default").await
	}

	/// Get all correlations from the default dictionary
	pub async fn get_correlations(&self) -> Result<Vec<Correlation>> {
		self.get_correlations_from_dictionary("default").await
	}

	/// Get correlation by ID from the default dictionary
	pub async fn get_correlation_by_id(&self, correlation_id: &CorrelationID) -> Result<Option<Correlation>> {
		self.get_correlation_by_id_from_dictionary(correlation_id, "default").await
	}

	/// Update correlation in the default dictionary
	pub async fn update_correlation(&self, correlation: &Correlation) -> Result<()> {
		self.update_correlation_in_dictionary(correlation, "default").await
	}

	/// Delete correlation from the default dictionary
	pub async fn delete_correlation(&self, correlation_id: &CorrelationID) -> Result<bool> {
		self.delete_correlation_from_dictionary(correlation_id, "default").await
	}
}
