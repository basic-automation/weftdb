use anyhow::Result;
use futures::StreamExt;

use crate::{AspectId, Correlation, CorrelationID, Database};
use super::Outputs;

/// Extension methods for correlation operations
/// 
/// These methods provide convenient access to correlations stored in the aspect's 
/// correlations database. They wrap the streaming `Outputs` trait methods to return
/// collected results.
impl Database {
	/// Get all correlations for an aspect
	///
	/// # Errors
	/// - if unable to retrieve correlations from database
	pub async fn get_correlations(&self, aspect_id: &AspectId) -> Result<Vec<Correlation>> {
		// Use the Outputs trait's streaming method and collect results
		let stream = <Self as Outputs>::get_correlations(self, aspect_id).await?;
		let correlations: Vec<Correlation> = stream
			.filter_map(|result| async move { result.ok() })
			.collect()
			.await;
		Ok(correlations)
	}

	/// Get correlation by ID
	///
	/// # Errors
	/// - if unable to retrieve correlation from database
	pub async fn get_correlation_by_id(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<Option<Correlation>> {
		match <Self as Outputs>::get_correlation(self, aspect_id, correlation_id).await {
			Ok(correlation) => Ok(Some(correlation)),
			Err(_) => Ok(None),
		}
	}
}
