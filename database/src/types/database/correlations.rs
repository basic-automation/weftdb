use anyhow::{bail, Result};
use uuid::Uuid;

use crate::{
	database::traits::AspectStructure, types::database::traits::{connection::Connection, database_structure::DatabaseStructure}, AspectId, Correlation, CorrelationID, Database
};

impl Database {
	/// Store correlation in a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert correlation
	pub async fn store_correlation_in_dictionary(&self, correlation: &Correlation) -> Result<()> {
		let aspect_id = correlation.aspect_id();
		let mut aspect = self.get_aspect(*aspect_id).await?;
		let correlations_db = aspect.correlations().await?;
		let correlation_path = aspect.correlations_path();

		let conn = Self::begin_concurrent(&correlations_db, &correlation_path, Some(self.cache.clone())).await?;

		// Serialize the correlation data
		// Convert HashMap<SignalType, ErrorRate> to HashMap<String, ErrorRate> for JSON serialization
		let error_rates_for_json: std::collections::HashMap<String, crate::types::signal::Distance> = correlation.error_rate().iter().map(|(signal_type, error_rate)| (signal_type.to_string(), error_rate.clone())).collect();
		let error_rate_json = serde_json::to_string(&error_rates_for_json).map_err(|e| anyhow::anyhow!(format!("Failed to serialize error rates: {e}")))?;
		let occurrences_json = serde_json::to_string(correlation.occurrences()).map_err(|e| anyhow::anyhow!(format!("Failed to serialize occurrences: {e}")))?;

		let correlation_id = correlation.id().to_string();

		// Insert or update correlation
		let current_timestamp = chrono::Utc::now().timestamp_millis();
		let res = conn.as_ref().execute("INSERT INTO correlations (id, dictionary_id, pattern_id, event_id, error_rates, occurrences, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", turso::params![correlation_id.clone(), correlation.dictionary_id().as_uuid().to_string(), correlation.pattern_id().to_string(), correlation.event_id().to_string(), error_rate_json.clone(), occurrences_json.clone(), current_timestamp, current_timestamp]).await;

		if res.is_ok() { 
                        println!("Correlation inserted successfully"); 
                } else {
			Self::rollback_concurrent(&conn).await?;
			bail!("Failed to insert correlation");
		}

		let _ = Self::commit_concurrent(&conn).await;
		Ok(())
	}

	/// Get correlations from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query correlations
	pub async fn get_correlations_from_dictionary(&self, aspect_id: &AspectId) -> Result<Vec<Correlation>> {
		let mut aspect = self.get_aspect(*aspect_id).await?;
		let correlations_turso_db = &aspect.correlations().await?;
		let correlation_db_path = &aspect.correlations_path();
		let conn = Self::begin_concurrent(correlations_turso_db, correlation_db_path, Some(self.cache.clone())).await?;

		let mut rows = conn.as_ref().query("SELECT id, dictionary_id, pattern_id, event_id, error_rates, occurrences FROM correlations ORDER BY updated_at DESC", turso::params![]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query correlations: {e}")))?;

		let mut correlations = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? {
			let correlation_id_str = Self::value_to_string(&row.get_value(0)?, "Correlation ID").await?;
			let dictionary_id_str = Self::value_to_string(&row.get_value(1)?, "Dictionary ID").await?;
			let pattern_id_str = Self::value_to_string(&row.get_value(2)?, "Pattern ID").await?;
			let event_id_str = Self::value_to_string(&row.get_value(3)?, "Event ID").await?;
			let error_rates_json = Self::value_to_string(&row.get_value(4)?, "Error rates").await?;
			let occurrences_json = Self::value_to_string(&row.get_value(5)?, "Occurrences").await?;

			let correlation_id = crate::CorrelationID::from_uuid(Uuid::parse_str(&correlation_id_str)?);
			let dictionary_id = crate::DictionaryId::from_uuid(Uuid::parse_str(&dictionary_id_str)?);
			let pattern_id = crate::PatternID::from_uuid(Uuid::parse_str(&pattern_id_str)?);
			let event_id = crate::EventID::from_uuid(Uuid::parse_str(&event_id_str)?);
			let subject_id = aspect.subject_id();

			// Deserialize from string-keyed HashMap back to SignalType-keyed HashMap
			let error_rates_from_json: std::collections::HashMap<String, crate::types::signal::Distance> = serde_json::from_str(&error_rates_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize error rates: {e}")))?;
			let mut error_rates = std::collections::HashMap::new();
			for (key_str, error_rate) in error_rates_from_json {
				if let Some(name) = key_str.strip_prefix("Custom(").and_then(|s| s.strip_suffix(')')) {
					error_rates.insert(crate::SignalType::Custom(name.to_string()), error_rate);
				} else {
					return Err(anyhow::anyhow!(format!("Invalid SignalType key: {key_str}")));
				}
			}
			let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize occurrences: {e}")))?;

			// Reconstruct the correlation using the with_id constructor
			let correlation = crate::Correlation::new(Some(correlation_id), dictionary_id, subject_id, aspect_id, pattern_id, event_id, error_rates, occurrences);
			correlations.push(correlation);
		}

		Ok(correlations)
	}

	/// Get correlation by ID from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query correlation
	pub async fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<Option<Correlation>> {
		let mut aspect = self.get_aspect(*aspect_id).await?;
		let correlations_db = &aspect.correlations().await?;
		let correlation_db_path = &aspect.correlations_path();
		let conn = Self::begin_concurrent(correlations_db, correlation_db_path, Some(self.cache.clone())).await?;
		let mut rows = conn.as_ref().query("SELECT id, dictionary_id, pattern_id, event_id, error_rates, occurrences FROM correlations WHERE id = ? LIMIT 1", turso::params![correlation_id.to_string()]).await.map_err(|e| anyhow::anyhow!(format!("Failed to query correlation: {e}")))?;

		let Some(row) = rows.next().await.map_err(|e| anyhow::anyhow!(format!("Failed to get row: {e}")))? else {
			Self::rollback_concurrent(&conn).await?;
			return Ok(None);
		};

		let _ = Self::commit_concurrent(&conn).await;

		let correlation_id_str = Self::value_to_string(&row.get_value(0)?, "Correlation ID").await?;
		let dictionary_id_str = Self::value_to_string(&row.get_value(1)?, "Dictionary ID").await?;
		let pattern_id_str = Self::value_to_string(&row.get_value(2)?, "Pattern ID").await?;
		let event_id_str = Self::value_to_string(&row.get_value(3)?, "Event ID").await?;
		let error_rates_json = Self::value_to_string(&row.get_value(4)?, "Error rates").await?;
		let occurrences_json = Self::value_to_string(&row.get_value(5)?, "Occurrences").await?;

		let correlation_id = crate::CorrelationID::from_uuid(Uuid::parse_str(&correlation_id_str)?);
		let dictionary_id = crate::DictionaryId::from_uuid(Uuid::parse_str(&dictionary_id_str)?);
		let pattern_id = crate::PatternID::from_uuid(Uuid::parse_str(&pattern_id_str)?);
		let event_id = crate::EventID::from_uuid(Uuid::parse_str(&event_id_str)?);

		// Deserialize from string-keyed HashMap back to SignalType-keyed HashMap
		let error_rates_from_json: std::collections::HashMap<String, crate::types::signal::Distance> = serde_json::from_str(&error_rates_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize error rates: {e}")))?;
		let mut error_rates = std::collections::HashMap::new();
		for (key_str, error_rate) in error_rates_from_json {
			if let Some(name) = key_str.strip_prefix("Custom(").and_then(|s| s.strip_suffix(')')) {
				error_rates.insert(crate::SignalType::Custom(name.to_string()), error_rate);
			} else {
				return Err(anyhow::anyhow!(format!("Invalid SignalType key: {key_str}")));
			}
		}
		let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| anyhow::anyhow!(format!("Failed to deserialize occurrences: {e}")))?;

		// Reconstruct the correlation using the with_id constructor
		let correlation = crate::Correlation::new(Some(correlation_id), dictionary_id, aspect.subject_id(), aspect_id, pattern_id, event_id, error_rates, occurrences);

		Ok(Some(correlation))
	}

	/// Update correlation in a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to update correlation
	pub async fn update_correlation_in_dictionary(&self, correlation: &Correlation) -> Result<()> {
		// For now, update is the same as store (UPSERT behavior)
		self.store_correlation_in_dictionary(correlation).await
	}

	/// Delete correlation from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete correlation
	pub async fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, aspect_id: &AspectId) -> Result<bool> {
		let mut aspect = self.get_aspect(*aspect_id).await?;
		let correlations_db = &aspect.correlations().await?;
		let correlation_db_path = &aspect.correlations_path();
		let conn = Self::begin_concurrent(correlations_db, correlation_db_path, Some(self.cache.clone())).await?;

		let res = conn.as_ref().execute("DELETE FROM correlations WHERE id = ?", turso::params![correlation_id.to_string()]).await;
		let rows_affected = match res {
			Ok(rows) => {
				println!("Correlation deleted successfully");
				rows
			}
			Err(e) => {
				Self::rollback_concurrent(&conn).await?;
				return Err(anyhow::anyhow!(format!("Failed to delete correlation: {e}")));
			}
		};
		let _ = Self::commit_concurrent(&conn).await;

		Ok(rows_affected > 0)
	}
}
