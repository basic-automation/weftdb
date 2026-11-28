//! Where a correlation's occurrences come from.
//!
//! A [`Correlation`] names its pattern (`pattern_id`) and holds no occurrences of its own:
//! the pattern's lossless [`OccurrenceLog`] in the dictionary is the one copy.
//! [`Database::correlation_occurrences`] resolves a set of correlations against the
//! dictionaries, loading each dictionary at most once, and falls back to the rows older
//! releases copied into `correlation_occurrences` for a correlation whose pattern is gone.
//! [`Database::retire_correlation_occurrences`] deletes those copied rows wherever the
//! dictionary already holds the same occurrences.

use std::{
	collections::{HashMap, HashSet}, sync::{Arc, LazyLock, Mutex, PoisonError}, time::SystemTime
};

use anyhow::Result;
use futures::TryStreamExt;

use crate::{
	cache, types::{
		database::{
			legacy_metadata_write, traits::{connection::Connection as _ConnectionTrait, database_structure::DatabaseStructure, outputs::Outputs, pipeline::PipelineOutputs}
		}, occurrence_codec::OccurrenceLog
	}, AspectId, Correlation, CorrelationID, Database, Error, Occurrence, Pattern, PatternID
};

/// Correlation ids per `IN (...)` statement and per cleanup transaction.
const ID_CHUNK: usize = 256;

/// One entry of the dictionaries directory as [`dictionary_fingerprint`] sees it.
type FingerprintEntry = (String, u64, Option<SystemTime>);

/// What earlier calls learned about one correlations store, so that orphan correlations
/// (whose pattern no dictionary holds) do not cost a full dictionary scan and a JSON
/// parse of their copied rows on every call.
#[derive(Default)]
struct OrphanCache {
	/// The dictionaries directory when `absent` was learned.
	fingerprint: Vec<FingerprintEntry>,
	/// Pattern ids no dictionary held at `fingerprint`.
	absent: HashSet<PatternID>,
	/// Copied rows already parsed, per correlation (`None`: it has none that parse).
	/// Nothing writes these rows any more, so an entry only goes stale by deletion.
	legacy: HashMap<CorrelationID, Option<Arc<OccurrenceLog>>>,
}

/// Per correlations database path.
static ORPHANS: LazyLock<Mutex<HashMap<String, OrphanCache>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_orphans<T>(key: &str, f: impl FnOnce(&mut OrphanCache) -> T) -> T {
	let mut guard = ORPHANS.lock().unwrap_or_else(PoisonError::into_inner);
	let out = f(guard.entry(key.to_string()).or_default());
	drop(guard);
	out
}

/// Every entry of the aspect's dictionaries directory (WAL and log files included) with
/// its length and modification time, sorted. Any dictionary write changes it.
async fn dictionary_fingerprint(database: &Database, aspect_id: &AspectId) -> Result<Vec<FingerprintEntry>> {
	use crate::types::database::traits::{aspect_structure::AspectStructure as _, config::Config as _};
	let aspect = database.get_aspect(aspect_id).await?;
	let path = Database::aspect_dictionaries_path(&database.name, aspect.subject_name(), aspect.name());
	drop(aspect);
	let mut out = Vec::new();
	let mut entries = match tokio::fs::read_dir(&path).await {
		Ok(entries) => entries,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
		Err(e) => return Err(e.into()),
	};
	while let Some(entry) = entries.next_entry().await? {
		let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
		out.push((entry.file_name().to_string_lossy().into_owned(), metadata.len(), metadata.modified().ok()));
	}
	out.sort_unstable();
	Ok(out)
}

/// The occurrences of a set of correlations, looked up by pattern id.
#[derive(Default)]
pub struct CorrelationOccurrences {
	patterns: Vec<Pattern>,
	by_id: HashMap<PatternID, usize>,
	legacy: HashMap<CorrelationID, Arc<OccurrenceLog>>,
	dictionaries_loaded: usize,
}

impl std::fmt::Debug for CorrelationOccurrences {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CorrelationOccurrences").field("patterns", &self.patterns.len()).field("legacy", &self.legacy.len()).field("dictionaries_loaded", &self.dictionaries_loaded).finish_non_exhaustive()
	}
}

impl CorrelationOccurrences {
	/// The occurrences of `correlation`: its pattern's log, or, for a correlation whose
	/// pattern no dictionary holds, the copy an older release stored with it.
	#[must_use]
	pub fn get(&self, correlation: &Correlation) -> Option<&OccurrenceLog> {
		self.by_id.get(correlation.pattern_id()).and_then(|&index| self.patterns.get(index)).map(Pattern::occurrence_log).or_else(|| self.legacy.get(correlation.id()).map(AsRef::as_ref))
	}

	/// The pattern `pattern_id` names, if a loaded dictionary holds it.
	#[must_use]
	pub fn pattern(&self, pattern_id: &PatternID) -> Option<&Pattern> {
		self.by_id.get(pattern_id).and_then(|&index| self.patterns.get(index))
	}

	/// How many dictionaries were read to build this.
	#[must_use]
	pub const fn dictionaries_loaded(&self) -> usize {
		self.dictionaries_loaded
	}

	/// How many correlations resolved only through their legacy copy.
	#[must_use]
	pub fn legacy_count(&self) -> usize {
		self.legacy.len()
	}

	/// Load `dictionary_name` and index the patterns in it that `wanted` names (all of them
	/// when `wanted` is `None`), removing them from `wanted`.
	async fn load(&mut self, database: &Database, aspect_id: &AspectId, dictionary_name: &str, mut wanted: Option<&mut HashSet<PatternID>>) -> Result<()> {
		let patterns: Vec<Pattern> = Outputs::get_dictionary_patterns(database, aspect_id, dictionary_name).await?.try_collect().await?;
		self.dictionaries_loaded += 1;
		for pattern in patterns {
			let id = pattern.id();
			if self.by_id.contains_key(&id) {
				continue;
			}
			if let Some(wanted) = wanted.as_deref_mut() {
				if !wanted.remove(&id) {
					continue;
				}
			}
			self.by_id.insert(id, self.patterns.len());
			self.patterns.push(pattern);
		}
		Ok(())
	}

	/// Resolve `wanted` against the pipeline's dictionaries first, then, only for ids still
	/// missing, every other dictionary file of the aspect, registered or not (a dictionary
	/// taken off the pipeline keeps its file). Each dictionary is read at most once. Returns whether every
	/// dictionary that was needed loaded.
	async fn resolve(&mut self, database: &Database, aspect_id: &AspectId, mut wanted: Option<HashSet<PatternID>>) -> bool {
		let mut complete = true;
		let mut seen: HashSet<String> = HashSet::new();
		let pipeline = match database.list_pipeline_dictionaries(aspect_id).await {
			Ok(names) => names,
			Err(e) => {
				tracing::warn!(error = %e, %aspect_id, "Failed to list pipeline dictionaries");
				Vec::new()
			}
		};
		for name in pipeline {
			if wanted.as_ref().is_some_and(HashSet::is_empty) {
				return complete;
			}
			if !seen.insert(name.clone()) {
				continue;
			}
			if let Err(e) = self.load(database, aspect_id, &name, wanted.as_mut()).await {
				tracing::warn!(error = %e, dictionary = name, "Failed to load dictionary while resolving correlation occurrences");
				complete = false;
			}
		}
		if wanted.as_ref().is_some_and(HashSet::is_empty) {
			return complete;
		}
		// Ids an earlier call already looked for everywhere, with no dictionary written
		// since, are not looked for again.
		let key = match (&wanted, database.get_correlations_db_path(aspect_id).await) {
			(Some(_), Ok(key)) => Some(key),
			_ => None,
		};
		let fingerprint = match &key {
			Some(_) => dictionary_fingerprint(database, aspect_id).await.ok(),
			None => None,
		};
		if let (Some(key), Some(fingerprint), Some(wanted)) = (&key, &fingerprint, &wanted) {
			if with_orphans(key, |cache| cache.fingerprint == *fingerprint && wanted.is_subset(&cache.absent)) {
				return complete;
			}
		}
		let complete_before = complete;
		match database.dictionary_file_names(aspect_id).await {
			Ok(all) => {
				for name in all {
					if wanted.as_ref().is_some_and(HashSet::is_empty) {
						break;
					}
					if !seen.insert(name.clone()) {
						continue;
					}
					if let Err(e) = self.load(database, aspect_id, &name, wanted.as_mut()).await {
						tracing::warn!(error = %e, dictionary = name, "Failed to load dictionary while resolving correlation occurrences");
						complete = false;
					}
				}
			}
			Err(e) => {
				tracing::warn!(error = %e, %aspect_id, "Failed to list dictionaries while resolving correlation occurrences");
				complete = false;
			}
		}
		// Only a full, clean scan is evidence that the remaining ids are nowhere.
		if let (true, true, Some(key), Some(fingerprint), Some(wanted)) = (complete, complete_before, key, fingerprint, wanted) {
			with_orphans(&key, |cache| {
				if cache.fingerprint != fingerprint {
					cache.fingerprint = fingerprint;
					cache.absent.clear();
				}
				cache.absent.extend(wanted);
			});
		}
		complete
	}
}

/// What [`Database::retire_correlation_occurrences`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetireReport {
	/// Correlations whose copied rows were deleted because their pattern holds them.
	pub deleted: usize,
	/// Correlations whose rows were kept because no dictionary holds their pattern.
	pub kept_orphan: usize,
	/// Correlations whose rows were kept because they are not a prefix of the pattern's log.
	pub kept_mismatch: usize,
	/// Row groups deleted because no correlation has their id.
	pub dangling: usize,
	/// Whether every dictionary loaded (when not, unresolved rows were only kept).
	pub complete: bool,
}

impl Database {
	/// The occurrences of `correlations`, resolved through their pattern ids.
	///
	/// # Errors
	///
	/// When reading the legacy `correlation_occurrences` rows fails. A dictionary that
	/// does not load is logged and skipped; its correlations resolve to nothing.
	pub async fn correlation_occurrences(&self, aspect_id: &AspectId, correlations: &[Correlation]) -> Result<CorrelationOccurrences> {
		let mut resolved = CorrelationOccurrences::default();
		if correlations.is_empty() {
			return Ok(resolved);
		}
		let wanted: HashSet<PatternID> = correlations.iter().map(|c| *c.pattern_id()).collect();
		resolved.resolve(self, aspect_id, Some(wanted)).await;
		let missing: Vec<&CorrelationID> = correlations.iter().filter(|c| resolved.pattern(c.pattern_id()).is_none()).map(Correlation::id).collect();
		if !missing.is_empty() {
			let db_path = self.get_correlations_db_path(aspect_id).await?;
			// Copied rows parsed by an earlier call are reused; only the rest are read.
			let unknown: Vec<&CorrelationID> = with_orphans(&db_path, |cache| {
				let mut unknown = Vec::new();
				for id in missing {
					match cache.legacy.get(id) {
						Some(Some(log)) => {
							resolved.legacy.insert(id.clone(), Arc::clone(log));
						}
						Some(None) => {}
						None => unknown.push(id),
					}
				}
				unknown
			});
			if !unknown.is_empty() {
				let db = self.get_correlations_db(aspect_id).await?;
				let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
				drop(db);
				let loaded = Self::read_copied_occurrences(&conn, &unknown).await;
				Self::commit_concurrent(&conn).await?;
				let mut loaded = loaded?;
				with_orphans(&db_path, |cache| {
					for id in unknown {
						let log = loaded.remove(id).map(|occurrences| Arc::new(occurrences.into_iter().collect::<OccurrenceLog>()));
						if let Some(log) = &log {
							resolved.legacy.insert(id.clone(), Arc::clone(log));
						}
						cache.legacy.insert(id.clone(), log);
					}
				});
			}
		}
		Ok(resolved)
	}

	/// The rows older releases copied into `correlation_occurrences` for `ids`, each id's
	/// list in `occurrence_index` order (a later row with the same index replaces an
	/// earlier one). An id with no rows is absent; one whose rows do not parse is logged
	/// and absent.
	async fn read_copied_occurrences(conn: &cache::Connection, ids: &[&CorrelationID]) -> Result<HashMap<CorrelationID, Vec<Occurrence>>> {
		let mut out: HashMap<CorrelationID, Vec<Occurrence>> = HashMap::new();
		for chunk in ids.chunks(ID_CHUNK) {
			let placeholders = vec!["?"; chunk.len()].join(", ");
			let params: Vec<String> = chunk.iter().map(ToString::to_string).collect();
			let sql = format!("SELECT correlation_id, occurrence_index, aspect_id, resolution, size, database_info, pattern_id, beginning_timestamp, end_timestamp FROM correlation_occurrences WHERE correlation_id IN ({placeholders})");
			let mut rows = conn.as_ref().query(&sql, turso::params_from_iter(params)).await.map_err(|e| Error::DatabaseError(format!("Failed to query correlation occurrences: {e}")))?;
			let mut by_id: HashMap<String, std::result::Result<HashMap<usize, Occurrence>, String>> = HashMap::new();
			loop {
				let Some(row) = rows.next().await.map_err(|e| Error::DatabaseError(format!("Failed to get correlation occurrence row: {e}")))? else { break };
				let Some(id) = row.get_value(0).ok().and_then(|v| v.as_text().cloned()) else { continue };
				let parsed = Self::parse_occurrence_row(&row);
				let entry = by_id.entry(id).or_insert_with(|| Ok(HashMap::new()));
				if let Ok(map) = entry {
					match parsed {
						Ok((index, occurrence)) => {
							map.insert(index, occurrence);
						}
						Err(e) => *entry = Err(e.to_string()),
					}
				}
			}
			drop(rows);
			for (id, parsed) in by_id {
				let Ok(uuid) = uuid::Uuid::parse_str(&id) else { continue };
				match parsed {
					Ok(map) => {
						let mut indexed: Vec<(usize, Occurrence)> = map.into_iter().collect();
						indexed.sort_unstable_by_key(|(index, _)| *index);
						out.insert(CorrelationID::from_uuid(uuid), indexed.into_iter().map(|(_, occurrence)| occurrence).collect());
					}
					Err(e) => tracing::warn!(correlation_id = id, error = e, "Failed to parse copied correlation occurrences"),
				}
			}
		}
		Ok(out)
	}

	/// Delete the occurrence rows older releases copied into `correlation_occurrences`
	/// wherever the dictionary already holds them.
	///
	/// A correlation's rows are deleted when its pattern resolves and the rows, in
	/// `occurrence_index` order, are a prefix of the pattern's log (pattern id,
	/// resolution, size and span at the millisecond precision the rows keep). Rows of a
	/// correlation that no longer exists are deleted. Rows whose pattern is gone, or that
	/// do not match, are kept: [`correlation_occurrences`](Self::correlation_occurrences)
	/// still reads them, and nothing writes them again. Each chunk of correlations is its
	/// own transaction and every step can run again, so an interrupted run is finished by
	/// the next.
	///
	/// # Errors
	///
	/// When the correlations database cannot be opened, read, or written.
	pub async fn retire_correlation_occurrences(&self, aspect_id: &AspectId) -> Result<RetireReport> {
		let mut report = RetireReport { complete: true, ..RetireReport::default() };
		let db = self.get_correlations_db(aspect_id).await?;
		let db_path = self.get_correlations_db_path(aspect_id).await?;

		// Which correlations have copied rows, and every correlation's pattern.
		let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
		let scanned = Self::scan_copied(&conn).await;
		Self::commit_concurrent(&conn).await?;
		drop(conn);
		let Some((with_rows, pattern_of)) = scanned? else {
			return Ok(report);
		};

		let mut resolver = CorrelationOccurrences::default();
		report.complete = resolver.resolve(self, aspect_id, None).await;

		let ids: Vec<CorrelationID> = with_rows.into_iter().collect();
		for chunk in ids.chunks(ID_CHUNK) {
			let refs: Vec<&CorrelationID> = chunk.iter().collect();
			let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
			let stored = Self::read_copied_occurrences(&conn, &refs).await;
			Self::commit_concurrent(&conn).await?;
			drop(conn);
			let stored = stored?;
			let mut delete: Vec<String> = Vec::new();
			for id in chunk {
				let Some(pattern_id) = pattern_of.get(id) else {
					report.dangling += 1;
					delete.push(id.to_string());
					continue;
				};
				let Some(pattern) = resolver.pattern(pattern_id) else {
					report.kept_orphan += 1;
					continue;
				};
				// Rows that did not parse are absent from `stored`; they are kept.
				let Some(rows) = stored.get(id) else {
					report.kept_mismatch += 1;
					continue;
				};
				if is_prefix(rows, pattern.occurrence_log()) {
					report.deleted += 1;
					delete.push(id.to_string());
				} else {
					report.kept_mismatch += 1;
				}
			}
			if delete.is_empty() {
				continue;
			}
			legacy_metadata_write(async {
				let conn = Self::begin_concurrent(&db, &db_path, Some(self.cache.clone())).await?;
				let placeholders = vec!["?"; delete.len()].join(", ");
				let sql = format!("DELETE FROM correlation_occurrences WHERE correlation_id IN ({placeholders})");
				if let Err(e) = conn.as_ref().execute(&sql, turso::params_from_iter(delete.clone())).await {
					Self::rollback_after_error(&conn).await;
					return Err(anyhow::anyhow!("Failed to delete copied correlation occurrences: {e}"));
				}
				Self::commit_concurrent(&conn).await
			})
			.await?;
			with_orphans(&db_path, |cache| {
				for id in chunk {
					cache.legacy.remove(id);
				}
			});
		}
		drop(resolver);
		drop(db);
		tracing::info!(%aspect_id, deleted = report.deleted, kept_orphan = report.kept_orphan, kept_mismatch = report.kept_mismatch, dangling = report.dangling, complete = report.complete, "Retired copied correlation occurrences");
		Ok(report)
	}

	/// The correlation ids `correlation_occurrences` holds rows for and every
	/// correlation's pattern id, or `None` when the table is empty.
	async fn scan_copied(conn: &cache::Connection) -> Result<Option<(HashSet<CorrelationID>, HashMap<CorrelationID, PatternID>)>> {
		let failed = |e: turso::Error| Error::DatabaseError(format!("Failed to scan correlation occurrences: {e}"));
		let mut rows = conn.as_ref().query("SELECT 1 FROM correlation_occurrences LIMIT 1", turso::params![]).await.map_err(failed)?;
		if rows.next().await.map_err(failed)?.is_none() {
			return Ok(None);
		}
		drop(rows);
		let mut with_rows = HashSet::new();
		let mut rows = conn.as_ref().query("SELECT DISTINCT correlation_id FROM correlation_occurrences", turso::params![]).await.map_err(failed)?;
		loop {
			let Some(row) = rows.next().await.map_err(failed)? else { break };
			if let Some(uuid) = row.get_value(0).ok().and_then(|v| v.as_text().and_then(|t| uuid::Uuid::parse_str(t).ok())) {
				with_rows.insert(CorrelationID::from_uuid(uuid));
			}
		}
		drop(rows);
		let mut pattern_of = HashMap::new();
		let mut rows = conn.as_ref().query("SELECT id, pattern_id FROM correlations", turso::params![]).await.map_err(failed)?;
		loop {
			let Some(row) = rows.next().await.map_err(failed)? else { break };
			let id = row.get_value(0).ok().and_then(|v| v.as_text().and_then(|t| uuid::Uuid::parse_str(t).ok()));
			let pattern = row.get_value(1).ok().and_then(|v| v.as_text().and_then(|t| uuid::Uuid::parse_str(t).ok()));
			if let (Some(id), Some(pattern)) = (id, pattern) {
				pattern_of.entry(CorrelationID::from_uuid(id)).or_insert_with(|| PatternID::from_uuid(pattern));
			}
		}
		drop(rows);
		Ok(Some((with_rows, pattern_of)))
	}
}

/// Whether `rows` are, in order, the first occurrences of `log`, compared as the rows
/// store them: pattern id, resolution, size, and span to the millisecond.
fn is_prefix(rows: &[Occurrence], log: &OccurrenceLog) -> bool {
	rows.len() <= log.len() && rows.iter().zip(log.iter()).all(|(a, b)| a.pattern_id() == b.pattern_id() && a.resolution() == b.resolution() && a.size() == b.size() && a.beginning().timestamp_millis() == b.beginning().timestamp_millis() && a.end().timestamp_millis() == b.end().timestamp_millis())
}
