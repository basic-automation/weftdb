use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::{AspectId, Correlation, CorrelationID, Database, Event, EventID, Pattern, PatternID, Subject, SubjectId};

/// Extension trait for event database operations
pub trait EventDatabase {
	/// Store an event in the database
	fn store_event(&self, event: &Event) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Get all unprocessed events from the database
	fn get_unprocessed_events(&self) -> impl std::future::Future<Output = Result<Vec<Event>>> + Send;

	/// Mark an event as processed
	fn mark_event_as_processed(&self, event: &Event) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Get processed events statistics
	fn get_processed_events_stats(&self) -> impl std::future::Future<Output = Result<(String, i64)>> + Send;

	/// Cleanup old processed events
	fn cleanup_processed_events(&self, days_old: i64) -> impl std::future::Future<Output = Result<u64>> + Send;

	/// Cleanup all processed events
	fn cleanup_all_processed_events(&self) -> impl std::future::Future<Output = Result<u64>> + Send;

	/// Get all events from database (including processed)
	fn get_all_events(&self) -> impl std::future::Future<Output = Result<Vec<Event>>> + Send;

	/// Get events queue for processing
	fn get_events_queue(&self) -> impl std::future::Future<Output = Result<Vec<Event>>> + Send;

	/// Dequeue a processed event
	fn dequeue_processed_event(&self, event: &Event) -> impl std::future::Future<Output = Result<bool>> + Send;

	/// Clear processed events queue
	fn clear_processed_events_queue(&self) -> impl std::future::Future<Output = Result<u64>> + Send;

	/// Clear all events
	fn clear_all_events(&self) -> impl std::future::Future<Output = Result<u64>> + Send;
}

/// Extension trait for pattern database operations
pub trait PatternDatabase {
	/// Store a pattern in the metadata database
	fn store_pattern(&self, pattern: &Pattern) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Store a pattern in a specific dictionary
	fn store_pattern_in_dictionary(&self, pattern: &Pattern, dictionary_name: &str) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Store multiple patterns in a dictionary
	fn store_patterns_in_dictionary(&self, patterns: &[Pattern], dictionary_name: &str, aspect_id: &AspectId) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Get patterns from a dictionary
	fn get_patterns_from_dictionary(&self, dictionary_name: &str, aspect_id: &AspectId) -> impl std::future::Future<Output = Result<Vec<Pattern>>> + Send;

	/// Get pattern by ID from dictionary
	fn get_pattern_by_id_from_dictionary(&self, pattern_id: &PatternID, dictionary_name: &str) -> impl std::future::Future<Output = Result<Option<Pattern>>> + Send;

	/// Delete pattern from dictionary
	fn delete_pattern_from_dictionary(&self, pattern_id: &PatternID, dictionary_name: &str) -> impl std::future::Future<Output = Result<bool>> + Send;
}

/// Extension trait for correlation database operations
pub trait CorrelationDatabase {
	/// Store a correlation in the database
	fn store_correlation(&self, correlation: &Correlation) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Store correlations in a specific dictionary
	fn store_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Get all correlations from the database
	fn get_correlations(&self) -> impl std::future::Future<Output = Result<Vec<Correlation>>> + Send;

	/// Get correlations from a specific dictionary
	fn get_correlations_from_dictionary(&self, dictionary_name: &str) -> impl std::future::Future<Output = Result<Vec<Correlation>>> + Send;

	/// Get correlation by ID
	fn get_correlation_by_id(&self, correlation_id: &CorrelationID) -> impl std::future::Future<Output = Result<Option<Correlation>>> + Send;

	/// Get correlation by ID from a specific dictionary
	fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> impl std::future::Future<Output = Result<Option<Correlation>>> + Send;

	/// Update correlation in the database
	fn update_correlation(&self, correlation: &Correlation) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Update correlation in a specific dictionary
	fn update_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Delete correlation from the database
	fn delete_correlation(&self, correlation_id: &CorrelationID) -> impl std::future::Future<Output = Result<bool>> + Send;

	/// Delete correlation from a specific dictionary
	fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> impl std::future::Future<Output = Result<bool>> + Send;
}

/// Extension trait for subject database operations
pub trait SubjectDatabase {
	/// Store a subject in the database
	fn store_subject(&self, subject: &Subject) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Get subject by ID
	fn get_subject_by_id(&self, subject_id: &SubjectId) -> impl std::future::Future<Output = Result<Option<Subject>>> + Send;

	/// Get all subjects
	fn get_all_subjects(&self) -> impl std::future::Future<Output = Result<Vec<Subject>>> + Send;

	/// Update subject
	fn update_subject(&self, subject: &Subject) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Delete subject
	fn delete_subject(&self, subject_id: &SubjectId) -> impl std::future::Future<Output = Result<bool>> + Send;
}

/// Extension trait for aspect database operations  
pub trait AspectDatabase {
	/// Store an aspect
	fn store_aspect(&self, aspect_id: &AspectId, subject_id: &SubjectId) -> impl std::future::Future<Output = Result<()>> + Send;

	/// Get aspect by ID
	fn get_aspect_by_id(&self, aspect_id: &AspectId) -> impl std::future::Future<Output = Result<Option<AspectId>>> + Send;

	/// Get aspects for subject
	fn get_aspects_for_subject(&self, subject_id: &SubjectId) -> impl std::future::Future<Output = Result<Vec<AspectId>>> + Send;

	/// Delete aspect
	fn delete_aspect(&self, aspect_id: &AspectId) -> impl std::future::Future<Output = Result<bool>> + Send;
}

// Implement all traits for Database
impl EventDatabase for Database {
	async fn store_event(&self, event: &Event) -> Result<()> {
		self.store_event(event).await
	}

	async fn get_unprocessed_events(&self) -> Result<Vec<Event>> {
		self.get_unprocessed_events().await
	}

	async fn mark_event_as_processed(&self, event: &Event) -> Result<()> {
		self.mark_event_as_processed(event).await
	}

	async fn get_processed_events_stats(&self) -> Result<(String, i64)> {
		self.get_processed_events_stats().await
	}

	async fn cleanup_processed_events(&self, days_old: i64) -> Result<u64> {
		self.cleanup_processed_events(days_old).await
	}

	async fn cleanup_all_processed_events(&self) -> Result<u64> {
		self.cleanup_all_processed_events().await
	}

	async fn get_all_events(&self) -> Result<Vec<Event>> {
		self.get_all_events().await
	}

	async fn get_events_queue(&self) -> Result<Vec<Event>> {
		self.get_events_queue().await
	}

	async fn dequeue_processed_event(&self, event: &Event) -> Result<bool> {
		self.dequeue_processed_event(event).await
	}

	async fn clear_processed_events_queue(&self) -> Result<u64> {
		self.clear_processed_events_queue().await
	}

	async fn clear_all_events(&self) -> Result<u64> {
		self.clear_all_events().await
	}
}

impl PatternDatabase for Database {
	async fn store_pattern(&self, pattern: &Pattern) -> Result<()> {
		self.store_pattern(pattern).await
	}

	async fn store_pattern_in_dictionary(&self, pattern: &Pattern, dictionary_name: &str) -> Result<()> {
		self.store_pattern_in_dictionary(pattern, dictionary_name).await
	}

	async fn store_patterns_in_dictionary(&self, patterns: &[Pattern], dictionary_name: &str, aspect_id: &AspectId) -> Result<()> {
		self.store_patterns_in_dictionary(patterns, dictionary_name, aspect_id).await
	}

	async fn get_patterns_from_dictionary(&self, dictionary_name: &str, aspect_id: &AspectId) -> Result<Vec<Pattern>> {
		self.get_patterns_from_dictionary(dictionary_name, aspect_id).await
	}

	async fn get_pattern_by_id_from_dictionary(&self, pattern_id: &PatternID, dictionary_name: &str) -> Result<Option<Pattern>> {
		self.get_pattern_by_id_from_dictionary(pattern_id, dictionary_name).await
	}

	async fn delete_pattern_from_dictionary(&self, pattern_id: &PatternID, dictionary_name: &str) -> Result<bool> {
		self.delete_pattern_from_dictionary(pattern_id, dictionary_name).await
	}
}

impl CorrelationDatabase for Database {
	async fn store_correlation(&self, correlation: &Correlation) -> Result<()> {
		self.store_correlation_in_dictionary(correlation, "default").await
	}

	async fn store_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> Result<()> {
		self.store_correlation_in_dictionary(correlation, dictionary_name).await
	}

	async fn get_correlations(&self) -> Result<Vec<Correlation>> {
		self.get_correlations_from_dictionary("default").await
	}

	async fn get_correlations_from_dictionary(&self, dictionary_name: &str) -> Result<Vec<Correlation>> {
		self.get_correlations_from_dictionary(dictionary_name).await
	}

	async fn get_correlation_by_id(&self, correlation_id: &CorrelationID) -> Result<Option<Correlation>> {
		self.get_correlation_by_id_from_dictionary(correlation_id, "default").await
	}

	async fn get_correlation_by_id_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> Result<Option<Correlation>> {
		self.get_correlation_by_id_from_dictionary(correlation_id, dictionary_name).await
	}

	async fn update_correlation(&self, correlation: &Correlation) -> Result<()> {
		self.update_correlation_in_dictionary(correlation, "default").await
	}

	async fn update_correlation_in_dictionary(&self, correlation: &Correlation, dictionary_name: &str) -> Result<()> {
		self.update_correlation_in_dictionary(correlation, dictionary_name).await
	}

	async fn delete_correlation(&self, correlation_id: &CorrelationID) -> Result<bool> {
		self.delete_correlation_from_dictionary(correlation_id, "default").await
	}

	async fn delete_correlation_from_dictionary(&self, correlation_id: &CorrelationID, dictionary_name: &str) -> Result<bool> {
		self.delete_correlation_from_dictionary(correlation_id, dictionary_name).await
	}
}
