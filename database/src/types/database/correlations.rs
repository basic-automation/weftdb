use anyhow::Result;

use crate::{types::database::traits::database_structure::DatabaseStructure, Correlation, CorrelationID};

impl super::Database {
	/// Get or create a dictionary correlations database (table should already exist)
	///
	/// # Errors
	/// - if unable to create database
	async fn get_or_create_dictionary_correlations_database(dictionary_name: &str, db_path: &str) -> Result<turso::Database> {
		let dictionary_dir = format!("{db_path}/dictionaries/{dictionary_name}");
		std::fs::create_dir_all(&dictionary_dir).map_err(|e| crate::Error::DatabaseError(format!("Failed to create dictionary directory: {e}")))?;

		let correlations_db_path = format!("{dictionary_dir}/correlations.db");
		let correlations_turso_db = match Self::get_turso_database(&correlations_db_path).await {
			Ok(db) => db,
			Err(_) => Self::create_turso_database(&correlations_db_path).await?,
		};

		// For non-default dictionaries, still create tables as needed
		if dictionary_name != "default" {
			// Ensure the correlations table exists for custom dictionaries
			let conn = correlations_turso_db.connect()?;
			conn.execute(
				"CREATE TABLE IF NOT EXISTS correlations (
					id TEXT PRIMARY KEY,
					dictionary_id TEXT NOT NULL,
					pattern_id TEXT NOT NULL,
					event_id TEXT NOT NULL,
					error_rates TEXT NOT NULL,
					occurrences TEXT NOT NULL,
					created_at INTEGER NOT NULL,
					updated_at INTEGER NOT NULL
				)",
				turso::params![],
			)
			.await
			.map_err(|e| crate::Error::DatabaseError(format!("Failed to create correlations table: {e}")))?;

			// Create indexes for efficient querying
			conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_dictionary ON correlations(dictionary_id)", turso::params![]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to create dictionary index: {e}")))?;
			conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_pattern ON correlations(pattern_id)", turso::params![]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to create pattern index: {e}")))?;
			conn.execute("CREATE INDEX IF NOT EXISTS idx_correlations_event ON correlations(event_id)", turso::params![]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to create event index: {e}")))?;
		}

		Ok(correlations_turso_db)
	}

	/// Store correlation in a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to insert correlation
	pub async fn store_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> Result<()> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = super::DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| crate::Error::DatabaseError("Database not found".to_string()))?
		};

		let correlations_turso_db = Self::get_or_create_dictionary_correlations_database(dictionary_name, db_info.path()).await?;

		// Serialize the correlation data
		// Convert HashMap<SignalType, ErrorRate> to HashMap<String, ErrorRate> for JSON serialization
		let error_rates_for_json: std::collections::HashMap<String, crate::types::signal::Distance> = correlation.error_rate().iter().map(|(signal_type, error_rate)| (signal_type.to_string(), error_rate.clone())).collect();
		let error_rate_json = serde_json::to_string(&error_rates_for_json).map_err(|e| crate::Error::DatabaseError(format!("Failed to serialize error rates: {e}")))?;
		let occurrences_json = serde_json::to_string(correlation.occurrences()).map_err(|e| crate::Error::DatabaseError(format!("Failed to serialize occurrences: {e}")))?;

		let correlation_id = correlation.id().to_string();
		let conn = correlations_turso_db.connect()?;

		// Insert or update correlation
		let current_timestamp = chrono::Utc::now().timestamp_millis();

		// Try to insert first
		let insert_result = conn.execute("INSERT INTO correlations (id, dictionary_id, pattern_id, event_id, error_rates, occurrences, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", turso::params![correlation_id.clone(), correlation.dictionary_id().as_uuid().to_string(), correlation.pattern_id().to_string(), correlation.event_id().to_string(), error_rate_json.clone(), occurrences_json.clone(), current_timestamp, current_timestamp]).await;

		// If insert fails due to duplicate, try update
		if insert_result.is_err() {
			conn.execute("UPDATE correlations SET error_rates = ?, occurrences = ?, updated_at = ? WHERE id = ?", turso::params![error_rate_json, occurrences_json, current_timestamp, correlation_id]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to update correlation: {e}")))?;
		}

		Ok(())
	}

	/// Get correlations from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query correlations
	pub async fn get_correlations_from_dictionary(&self, dictionary_name: &str) -> Result<Vec<Correlation>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = super::DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| crate::Error::DatabaseError("Database not found".to_string()))?
		};

		let correlations_turso_db = Self::get_or_create_dictionary_correlations_database(dictionary_name, db_info.path()).await?;
		let conn = correlations_turso_db.connect()?;

		let mut rows = conn.query("SELECT id, dictionary_id, pattern_id, event_id, error_rates, occurrences FROM correlations ORDER BY updated_at DESC", turso::params![]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to query correlations: {e}")))?;

		let mut correlations = Vec::new();
		while let Some(row) = rows.next().await.map_err(|e| crate::Error::DatabaseError(format!("Failed to get row: {e}")))? {
			let correlation_id_str = super::value_to_string(row.get_value(0)?, "Correlation ID")?;
			let dictionary_id_str = super::value_to_string(row.get_value(1)?, "Dictionary ID")?;
			let pattern_id_str = super::value_to_string(row.get_value(2)?, "Pattern ID")?;
			let event_id_str = super::value_to_string(row.get_value(3)?, "Event ID")?;
			let error_rates_json = super::value_to_string(row.get_value(4)?, "Error rates")?;
			let occurrences_json = super::value_to_string(row.get_value(5)?, "Occurrences")?;

			let correlation_id = crate::CorrelationID::from_uuid(uuid::Uuid::parse_str(&correlation_id_str)?);
			let dictionary_id = crate::DictionaryId::from_uuid(uuid::Uuid::parse_str(&dictionary_id_str)?);
			let pattern_id = crate::PatternID::from_uuid(uuid::Uuid::parse_str(&pattern_id_str)?);
			let event_id = crate::EventID::from_uuid(uuid::Uuid::parse_str(&event_id_str)?);

			// Deserialize from string-keyed HashMap back to SignalType-keyed HashMap
			let error_rates_from_json: std::collections::HashMap<String, crate::types::signal::Distance> = serde_json::from_str(&error_rates_json).map_err(|e| crate::Error::DatabaseError(format!("Failed to deserialize error rates: {e}")))?;
			let mut error_rates = std::collections::HashMap::new();
			for (key_str, error_rate) in error_rates_from_json {
				if let Some(name) = key_str.strip_prefix("Custom(").and_then(|s| s.strip_suffix(')')) {
					error_rates.insert(crate::SignalType::Custom(name.to_string()), error_rate);
				} else {
					return Err(crate::Error::DatabaseError(format!("Invalid SignalType key: {key_str}")).into());
				}
			}
			let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| crate::Error::DatabaseError(format!("Failed to deserialize occurrences: {e}")))?;

			// Reconstruct the correlation using the with_id constructor
			let correlation = crate::Correlation::with_id(correlation_id, dictionary_id, pattern_id, event_id, error_rates, occurrences);
			correlations.push(correlation);
		}

		Ok(correlations)
	}

	/// Get correlation by ID from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to query correlation
	pub async fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> Result<Option<Correlation>> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = super::DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| crate::Error::DatabaseError("Database not found".to_string()))?
		};

		let correlations_turso_db = Self::get_or_create_dictionary_correlations_database(dictionary_name, db_info.path()).await?;
		let conn = correlations_turso_db.connect()?;

		let mut rows = conn.query("SELECT id, dictionary_id, pattern_id, event_id, error_rates, occurrences FROM correlations WHERE id = ? LIMIT 1", turso::params![correlation_id.to_string()]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to query correlation: {e}")))?;

		let Some(row) = rows.next().await.map_err(|e| crate::Error::DatabaseError(format!("Failed to get row: {e}")))? else {
			return Ok(None);
		};

		let correlation_id_str = super::value_to_string(row.get_value(0)?, "Correlation ID")?;
		let dictionary_id_str = super::value_to_string(row.get_value(1)?, "Dictionary ID")?;
		let pattern_id_str = super::value_to_string(row.get_value(2)?, "Pattern ID")?;
		let event_id_str = super::value_to_string(row.get_value(3)?, "Event ID")?;
		let error_rates_json = super::value_to_string(row.get_value(4)?, "Error rates")?;
		let occurrences_json = super::value_to_string(row.get_value(5)?, "Occurrences")?;

		let correlation_id = crate::CorrelationID::from_uuid(uuid::Uuid::parse_str(&correlation_id_str)?);
		let dictionary_id = crate::DictionaryId::from_uuid(uuid::Uuid::parse_str(&dictionary_id_str)?);
		let pattern_id = crate::PatternID::from_uuid(uuid::Uuid::parse_str(&pattern_id_str)?);
		let event_id = crate::EventID::from_uuid(uuid::Uuid::parse_str(&event_id_str)?);

		// Deserialize from string-keyed HashMap back to SignalType-keyed HashMap
		let error_rates_from_json: std::collections::HashMap<String, crate::types::signal::Distance> = serde_json::from_str(&error_rates_json).map_err(|e| crate::Error::DatabaseError(format!("Failed to deserialize error rates: {e}")))?;
		let mut error_rates = std::collections::HashMap::new();
		for (key_str, error_rate) in error_rates_from_json {
			if let Some(name) = key_str.strip_prefix("Custom(").and_then(|s| s.strip_suffix(')')) {
				error_rates.insert(crate::SignalType::Custom(name.to_string()), error_rate);
			} else {
				return Err(crate::Error::DatabaseError(format!("Invalid SignalType key: {key_str}")).into());
			}
		}
		let occurrences: Vec<crate::Occurrence> = serde_json::from_str(&occurrences_json).map_err(|e| crate::Error::DatabaseError(format!("Failed to deserialize occurrences: {e}")))?;

		// Reconstruct the correlation using the with_id constructor
		let correlation = crate::Correlation::with_id(correlation_id, dictionary_id, pattern_id, event_id, error_rates, occurrences);

		Ok(Some(correlation))
	}

	/// Update correlation in a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to update correlation
	pub async fn update_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> Result<()> {
		// For now, update is the same as store (UPSERT behavior)
		self.store_correlation_in_dictionary(correlation, dictionary_name).await
	}

	/// Delete correlation from a specific dictionary
	///
	/// # Errors
	/// - if database not found
	/// - if unable to delete correlation
	pub async fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> Result<bool> {
		// Get database info with proper validation
		let db_id = self.id();
		let db_info = {
			let databases = super::DATABASES.lock().await;
			databases.get(&db_id).cloned().ok_or_else(|| crate::Error::DatabaseError("Database not found".to_string()))?
		};

		let correlations_turso_db = Self::get_or_create_dictionary_correlations_database(dictionary_name, db_info.path()).await?;
		let conn = correlations_turso_db.connect()?;

		let result = conn.execute("DELETE FROM correlations WHERE id = ?", turso::params![correlation_id.to_string()]).await.map_err(|e| crate::Error::DatabaseError(format!("Failed to delete correlation: {e}")))?;

		Ok(result > 0)
	}
}
