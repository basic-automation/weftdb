use std::{
	sync::{
		atomic::{AtomicBool, Ordering}, Arc
	}, time::Instant
};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use crossterm::event::{KeyCode, KeyEventKind};
use futures::StreamExt;
use num_traits::cast::ToPrimitive;
use ratatui::widgets::ListState;
use splimes::Spline;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use weftdb::{
	database::traits::{AspectStructure, DatabaseStructure, Inputs, Outputs}, AggressivenessScaling, Aspect, CompressionConfig, Database, DatasetId, InputMeasurement, Resolution, SizeBasedCompressionConfig, Subject, TimeBasedCompressionConfig
};

use crate::logging::LogBuffer;

/// Resolution steps from finest to coarsest for +/- navigation
const RESOLUTION_ORDER: [Resolution; 10] = [Resolution::Nanoseconds, Resolution::Microseconds, Resolution::Milliseconds, Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days, Resolution::Weeks, Resolution::Months, Resolution::Years];

#[derive(Debug, Clone)]
pub enum AppState {
	SelectingMode,
	EnteringDatabaseName {
		input: String,
		cursor_position: usize,
		validation_error: Option<String>,
	},
	CreatingDatabase {
		name: String,
	},
	EnteringSubjectName {
		input: String,
		cursor_position: usize,
		validation_error: Option<String>,
	},
	CreatingSubject {
		name: String,
	},
	EnteringAspectName {
		input: String,
		cursor_position: usize,
		validation_error: Option<String>,
		selected_resolution: usize,
	},
	CreatingAspect {
		name: String,
		resolution: Resolution,
	},
	EnteringCsvPath {
		input: String,
		cursor_position: usize,
		validation_error: Option<String>,
	},
	SelectingDatabase,
	SelectingSubject,
	SelectingAspect,
	Loading {
		loaded_count: usize,
		total_scanned: usize,
		status: String,
	},
	Plotting,
	/// Step 1: Choose compression mode (with presets)
	SelectingCompressionMode {
		selected_index: usize,
	},
	/// Step 2: Configure details for the selected mode
	ConfiguringCompression {
		mode: CompressionMode,
		// Time-based fields
		time_pure_days: String,
		time_tier_days: String,
		time_max_tiers: String,
		time_scaling_index: usize, // 0=Linear, 1=Exponential
		// Size-based fields
		size_target_gb: String,
		size_min_agg: String,
		size_max_agg: String,
		// Common
		base_resolution_index: usize,
		focused_field: CompressionField,
		validation_error: Option<String>,
	},
	Error(String),
}

/// Compression mode type
#[derive(Debug, Clone, PartialEq)]
pub enum CompressionMode {
	TimeBased,
	SizeBased,
	Combined,
}

/// Fields in the compression configuration form
#[derive(Debug, Clone, PartialEq)]
pub enum CompressionField {
	TimePureDays,
	TimeTierDays,
	TimeMaxTiers,
	TimeScaling,
	SizeTargetGb,
	SizeMinAgg,
	SizeMaxAgg,
	BaseResolution,
}

/// Information about the last compression run for UI display
#[derive(Debug, Clone)]
pub struct LastCompressionInfo {
	/// When the compression completed
	pub completed_at: DateTime<Utc>,
	/// Number of measurements before compression
	pub original_count: usize,
	/// Number of measurements after compression
	pub compressed_count: usize,
	/// Overall compression ratio
	pub compression_ratio: f64,
	/// Number of time-based tiers processed
	pub time_based_tiers: usize,
	/// Duration of compression in milliseconds
	pub duration_ms: u64,
}

/// Compression status displayed in side pane
#[derive(Debug, Clone)]
pub enum CompressionStatus {
	/// Compression is running
	Running { phase: String, progress_percent: Option<u8>, current_tier: Option<u32>, total_tiers: Option<u32>, aggressiveness: Option<f64>, time_range: Option<(DateTime<Utc>, DateTime<Utc>)> },
	/// Compression completed successfully
	Complete { original_count: usize, compressed_count: usize, compression_ratio: f64, final_size_bytes: u64, time_based_tiers: usize, size_based_iterations: usize, duration_ms: u64 },
	/// Compression failed
	Error(String),
	/// Idle - showing last compression stats (default state when aspect selected)
	Idle { last_compression: Option<LastCompressionInfo>, dirty_regions_count: usize },
}

/// Messages from background compression task
pub enum CompressionMessage {
	Progress {
		phase: String,
		progress_percent: Option<u8>,
		current_tier: Option<u32>,
		total_tiers: Option<u32>,
		aggressiveness: Option<f64>,
		time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
	},
	Complete {
		original_count: usize,
		compressed_count: usize,
		compression_ratio: f64,
		final_size_bytes: u64,
		time_based_tiers: usize,
		size_based_iterations: usize,
		duration_ms: u64,
	},
	Error(String),
	/// Stats loaded for idle display
	StatsLoaded {
		last_compression: Option<LastCompressionInfo>,
		dirty_regions_count: usize,
	},
}

pub struct App {
	pub state: AppState,
	pub databases: Vec<String>,
	pub subjects: Vec<Subject>,
	pub aspects: Vec<Aspect>,
	pub selected_database: Option<String>,
	pub selected_subject: Option<Subject>,
	pub selected_aspect: Option<Aspect>,
	pub measurements: Vec<(DateTime<Utc>, f64)>,
	pub view_resolution: Resolution, // Current analysis resolution for viewing
	pub list_state: ListState,
	pub mode_list_state: ListState,
	pub database_name_input: String,
	pub database_name_cursor: usize,
	pub error_message: Option<String>,
	// Background loading state
	loading_receiver: Option<mpsc::Receiver<LoadingMessage>>,
	loading_cancel: Option<Arc<AtomicBool>>,
	// CSV import progress (shown as overlay on plotting screen)
	pub csv_import_progress: Option<(usize, usize, String)>, // (loaded, total, status)
	// Logging
	pub log_buffer: LogBuffer,
	// Streaming indicator for incremental plot updates
	pub loading_in_progress: bool,
	// Compression state (displayed as side pane on Plotting screen)
	pub compression_status: Option<CompressionStatus>,
	pub compression_list_state: ListState,
	compression_receiver: Option<mpsc::Receiver<CompressionMessage>>,
	compression_cancel: Option<Arc<AtomicBool>>,
}

/// Messages sent from background loading task
pub enum LoadingMessage {
	Progress {
		loaded: usize,
		scanned: usize,
		status: String,
	},
	UpdateMeasurements(Vec<(DateTime<Utc>, f64)>), // Update plot with newly imported measurements
	CompleteMeasurements(Vec<(DateTime<Utc>, f64)>),
	/// Signal the default resolution calculated from dataset span
	DefaultResolution(Resolution),
	Subjects(Vec<Subject>),
	Aspects(Vec<Aspect>),
	DatabaseSkipped,
	Error(String),
}

impl App {
	pub async fn new(log_buffer: LogBuffer) -> Self {
		let databases = match list_databases().await {
			Ok(dbs) => dbs,
			Err(e) => return Self { state: AppState::Error(format!("Failed to list databases: {}", e)), databases: vec![], subjects: vec![], aspects: vec![], selected_database: None, selected_subject: None, selected_aspect: None, measurements: vec![], view_resolution: Resolution::Days, list_state: ListState::default(), mode_list_state: ListState::default(), database_name_input: String::new(), database_name_cursor: 0, error_message: None, loading_receiver: None, loading_cancel: None, csv_import_progress: None, log_buffer, loading_in_progress: false, compression_status: None, compression_list_state: ListState::default(), compression_receiver: None, compression_cancel: None },
		};
		let mut mode_list_state = ListState::default();
		mode_list_state.select(Some(0));

		Self {
			state: AppState::SelectingMode,
			databases,
			subjects: vec![],
			aspects: vec![],
			selected_database: None,
			selected_subject: None,
			selected_aspect: None,
			measurements: vec![],
			view_resolution: Resolution::Days, // Will be recalculated when data is loaded
			list_state: ListState::default(),
			mode_list_state,
			database_name_input: String::new(),
			database_name_cursor: 0,
			error_message: None,
			loading_receiver: None,
			loading_cancel: None,
			csv_import_progress: None,
			log_buffer,
			loading_in_progress: false,
			compression_status: None,
			compression_list_state: ListState::default(),
			compression_receiver: None,
			compression_cancel: None,
		}
	}

	/// Get the next finer resolution (zoom in), bounded by the aspect's native resolution
	fn get_finer_resolution(&self, native_resolution: Resolution) -> Option<Resolution> {
		let current_idx = RESOLUTION_ORDER.iter().position(|r| *r == self.view_resolution)?;
		let native_idx = RESOLUTION_ORDER.iter().position(|r| *r == native_resolution)?;

		// Can't go finer than native resolution
		if current_idx <= native_idx {
			return None;
		}

		Some(RESOLUTION_ORDER[current_idx - 1])
	}

	/// Get the next coarser resolution (zoom out), up to Years
	fn get_coarser_resolution(&self) -> Option<Resolution> {
		let current_idx = RESOLUTION_ORDER.iter().position(|r| *r == self.view_resolution)?;

		// Can't go coarser than Years (last in the array)
		if current_idx >= RESOLUTION_ORDER.len() - 1 {
			return None;
		}

		Some(RESOLUTION_ORDER[current_idx + 1])
	}

	/// Poll for background loading progress - call this from the event loop
	pub fn poll_loading(&mut self) {
		let mut should_clear = false;
		let mut new_measurements = None;
		let mut new_state = None;
		let mut should_load_compression_stats = false;

		if let Some(ref mut rx) = self.loading_receiver {
			// Non-blocking receive
			while let Ok(msg) = rx.try_recv() {
				match msg {
					LoadingMessage::Progress { loaded, scanned, status } => {
						// If we're on the plotting screen with an import in progress, update csv_import_progress
						if matches!(self.state, AppState::Plotting) && self.csv_import_progress.is_some() {
							self.csv_import_progress = Some((loaded, scanned, status));
						} else {
							// For other loading operations, use the Loading state
							new_state = Some(AppState::Loading { loaded_count: loaded, total_scanned: scanned, status });
						}
					}
					LoadingMessage::UpdateMeasurements(measurements) => {
						// Update plot with newly imported measurements while keeping import progress visible
						if matches!(self.state, AppState::Plotting) && self.csv_import_progress.is_some() {
							self.measurements = measurements;
						} else if matches!(self.state, AppState::Loading { .. }) {
							// Streaming preview - update measurements and show plot early
							self.measurements = measurements;
							new_state = Some(AppState::Plotting);
						} else {
							new_measurements = Some(measurements);
						}
					}
					LoadingMessage::CompleteMeasurements(measurements) => {
						new_measurements = Some(measurements.clone());
						self.loading_in_progress = false;
						// Clear CSV import progress when done
						if matches!(self.state, AppState::Plotting) {
							self.csv_import_progress = None;
						} else {
							new_state = Some(AppState::Plotting);
						}
						should_clear = true;
						// Load compression stats for the pane now that measurements are loaded
						should_load_compression_stats = true;
					}
					LoadingMessage::Subjects(subjects) => {
						// set subjects and transition to selecting subject
						self.subjects = subjects;
						new_state = Some(AppState::SelectingSubject);
						self.list_state.select(Some(0));
						should_clear = true;
					}
					LoadingMessage::Aspects(aspects) => {
						self.aspects = aspects;
						new_state = Some(AppState::SelectingAspect);
						self.list_state.select(Some(0));
						should_clear = true;
					}
					LoadingMessage::DefaultResolution(resolution) => {
						// Set the view resolution to the calculated default
						self.view_resolution = resolution;
					}
					LoadingMessage::DatabaseSkipped => {
						// Database was locked, will retry on next iteration
						// No action needed - the loop will naturally retry after debounce
						debug!("Database lock detected, plot update skipped");
					}
					LoadingMessage::Error(e) => {
						// Clear CSV import progress on error
						self.csv_import_progress = None;
						self.loading_in_progress = false;
						new_state = Some(AppState::Error(format!("Failed to load measurements: {}", e)));
						should_clear = true;
					}
				}
			}
		}

		if let Some(measurements) = new_measurements {
			self.measurements = measurements;
		}
		if let Some(state) = new_state {
			self.state = state;
		}
		if should_clear {
			self.loading_receiver = None;
			self.loading_cancel = None;
		}

		// Load compression stats after borrow ends
		if should_load_compression_stats {
			self.start_loading_compression_stats();
		}

		// Also poll compression progress
		self.poll_compression();
	}

	pub async fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		// Global quit: allow 'q' or 'Q' to exit from any state (except text input states)
		if key.kind == KeyEventKind::Press {
			if let KeyCode::Char(c) = key.code {
				if (c == 'q' || c == 'Q') && !matches!(self.state, AppState::EnteringDatabaseName { .. } | AppState::EnteringSubjectName { .. } | AppState::EnteringAspectName { .. } | AppState::EnteringCsvPath { .. } | AppState::ConfiguringCompression { .. }) {
					// Cancel any background loading
					if let Some(cancel) = &self.loading_cancel {
						cancel.store(true, Ordering::SeqCst);
					}
					return Ok(true);
				}
			}
		}
		match self.state.clone() {
			AppState::SelectingMode => self.handle_mode_selection(key).await,
			AppState::EnteringDatabaseName { .. } => self.handle_database_name_input(key).await,
			AppState::CreatingDatabase { .. } => self.handle_creating_database(key).await,
			AppState::EnteringSubjectName { .. } => self.handle_subject_name_input(key).await,
			AppState::CreatingSubject { .. } => self.handle_creating_subject(key).await,
			AppState::EnteringAspectName { .. } => self.handle_aspect_name_input(key).await,
			AppState::CreatingAspect { .. } => self.handle_creating_aspect(key).await,
			AppState::EnteringCsvPath { .. } => self.handle_csv_path_input(key).await,
			AppState::SelectingDatabase => self.handle_database_selection(key).await,
			AppState::SelectingSubject => self.handle_subject_selection(key).await,
			AppState::SelectingAspect => self.handle_aspect_selection(key).await,
			AppState::Loading { .. } => self.handle_loading(key).await,
			AppState::Plotting => self.handle_plotting(key).await,
			AppState::SelectingCompressionMode { .. } => self.handle_compression_mode_selection(key).await,
			AppState::ConfiguringCompression { .. } => self.handle_compression_config(key).await,
			AppState::Error(_) => {
				if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
					self.refresh_database_list().await;
					self.state = AppState::SelectingDatabase;
					self.selected_database = None;
					self.selected_subject = None;
					self.selected_aspect = None;
					self.subjects.clear();
					self.aspects.clear();
					self.list_state.select(Some(0));
				}
				Ok(false)
			}
		}
	}

	async fn handle_loading(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}
		// Allow canceling the load with Esc
		if key.code == KeyCode::Esc {
			if let Some(cancel) = &self.loading_cancel {
				cancel.store(true, Ordering::SeqCst);
			}
			self.loading_receiver = None;
			self.loading_cancel = None;
			self.state = AppState::SelectingAspect;
			self.list_state.select(Some(0));
		}
		Ok(false)
	}

	async fn handle_mode_selection(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		match key.code {
			KeyCode::Down => {
				let i = self.mode_list_state.selected().unwrap_or(0);
				if i < 1 {
					self.mode_list_state.select(Some(i + 1));
				}
			}
			KeyCode::Up => {
				let i = self.mode_list_state.selected().unwrap_or(0);
				if i > 0 {
					self.mode_list_state.select(Some(i - 1));
				}
			}
			KeyCode::Enter => {
				match self.mode_list_state.selected() {
					Some(0) => {
						// Create New Database
						self.state = AppState::EnteringDatabaseName { input: String::new(), cursor_position: 0, validation_error: None };
					}
					Some(1) => {
						// Load Existing Database
						if self.databases.is_empty() {
							self.state = AppState::Error("No databases found".to_string());
						} else {
							self.state = AppState::SelectingDatabase;
							self.list_state.select(Some(0));
						}
					}
					_ => {}
				}
			}
			_ => {}
		}
		Ok(false)
	}

	async fn handle_database_name_input(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		let (mut input, mut cursor, _) = match &self.state {
			AppState::EnteringDatabaseName { input, cursor_position, validation_error } => (input.clone(), *cursor_position, validation_error.clone()),
			_ => return Ok(false),
		};

		match key.code {
			KeyCode::Char(c) => {
				// Only alphanumeric, underscore, hyphen
				if c.is_alphanumeric() || c == '_' || c == '-' {
					input.insert(cursor, c);
					cursor += 1;
				}
			}
			KeyCode::Backspace => {
				if cursor > 0 {
					input.remove(cursor - 1);
					cursor -= 1;
				}
			}
			KeyCode::Delete => {
				if cursor < input.len() {
					input.remove(cursor);
				}
			}
			KeyCode::Left => cursor = cursor.saturating_sub(1),
			KeyCode::Right => cursor = cursor.min(input.len()),
			KeyCode::Home => cursor = 0,
			KeyCode::End => cursor = input.len(),
			KeyCode::Enter => {
				// Validate
				if let Some(error) = validate_database_name(&input, &self.databases) {
					self.state = AppState::EnteringDatabaseName { input, cursor_position: cursor, validation_error: Some(error) };
					return Ok(false);
				}

				// Start creation
				self.start_creating_database(input.clone());
				self.state = AppState::CreatingDatabase { name: input };
				return Ok(false);
			}
			KeyCode::Esc => {
				self.state = AppState::SelectingMode;
				self.mode_list_state.select(Some(0));
				return Ok(false);
			}
			_ => {}
		}

		// Update state if still in input mode
		self.state = AppState::EnteringDatabaseName { input, cursor_position: cursor, validation_error: None };

		Ok(false)
	}

	async fn handle_creating_database(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
			if let Some(cancel) = &self.loading_cancel {
				cancel.store(true, Ordering::SeqCst);
			}
			self.loading_receiver = None;
			self.loading_cancel = None;
			self.state = AppState::SelectingMode;
			self.mode_list_state.select(Some(0));
		}
		Ok(false)
	}

	fn start_creating_database(&mut self, db_name: String) {
		debug!("Starting database creation: {}", db_name);

		let (tx, rx) = mpsc::channel(10);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel);

		// Set selected database immediately - will be available for subject creation
		self.selected_database = Some(db_name.clone());
		debug!("Selected database set to: {}", db_name);

		tokio::spawn(async move {
			debug!("Database creation task started for: {}", db_name);
			match Database::new(&db_name).await {
				Ok(_) => {
					info!("Database created successfully: {}", db_name);
					let _ = tx.send(LoadingMessage::Subjects(vec![])).await;
				}
				Err(e) => {
					error!("Failed to create database '{}': {}", db_name, e);
					let _ = tx.send(LoadingMessage::Error(format!("Failed to create: {}", e))).await;
				}
			}
		});
	}

	async fn refresh_database_list(&mut self) {
		match list_databases().await {
			Ok(dbs) => {
				debug!("Refreshed database list: {} databases found", dbs.len());
				self.databases = dbs;
			}
			Err(e) => {
				debug!("Failed to refresh database list: {}", e);
			}
		}
	}

	async fn handle_subject_name_input(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		let (mut input, mut cursor, _) = match &self.state {
			AppState::EnteringSubjectName { input, cursor_position, validation_error } => (input.clone(), *cursor_position, validation_error.clone()),
			_ => return Ok(false),
		};

		match key.code {
			KeyCode::Char(c) => {
				if !c.is_control() {
					input.insert(cursor, c);
					cursor += 1;
				}
			}
			KeyCode::Backspace => {
				if cursor > 0 {
					input.remove(cursor - 1);
					cursor -= 1;
				}
			}
			KeyCode::Delete => {
				if cursor < input.len() {
					input.remove(cursor);
				}
			}
			KeyCode::Left => cursor = cursor.saturating_sub(1),
			KeyCode::Right => cursor = cursor.min(input.len()),
			KeyCode::Home => cursor = 0,
			KeyCode::End => cursor = input.len(),
			KeyCode::Enter => {
				debug!("Enter pressed for subject name: '{}'", input);
				if let Some(error) = validate_subject_name(&input) {
					warn!("Subject name validation failed: {}", error);
					self.state = AppState::EnteringSubjectName { input, cursor_position: cursor, validation_error: Some(error) };
					return Ok(false);
				}
				info!("Valid subject name, starting creation process");
				self.start_creating_subject(input.clone());
				self.state = AppState::CreatingSubject { name: input };
				return Ok(false);
			}
			KeyCode::Esc => {
				self.state = AppState::SelectingSubject;
				return Ok(false);
			}
			_ => {}
		}

		self.state = AppState::EnteringSubjectName { input, cursor_position: cursor, validation_error: None };

		Ok(false)
	}

	async fn handle_creating_subject(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
			if let Some(cancel) = &self.loading_cancel {
				cancel.store(true, Ordering::SeqCst);
			}
			self.loading_receiver = None;
			self.loading_cancel = None;
			self.state = AppState::SelectingSubject;
		}
		Ok(false)
	}

	async fn handle_aspect_name_input(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		let (mut input, mut cursor, _, mut selected_resolution) = match &self.state {
			AppState::EnteringAspectName { input, cursor_position, validation_error, selected_resolution } => (input.clone(), *cursor_position, validation_error.clone(), *selected_resolution),
			_ => return Ok(false),
		};

		let resolutions = vec![Resolution::Nanoseconds, Resolution::Microseconds, Resolution::Milliseconds, Resolution::Seconds, Resolution::Minutes, Resolution::Hours, Resolution::Days, Resolution::Weeks, Resolution::Months, Resolution::Years];

		match key.code {
			KeyCode::Char(c) => {
				if !c.is_control() {
					input.insert(cursor, c);
					cursor += 1;
				}
			}
			KeyCode::Backspace => {
				if cursor > 0 {
					input.remove(cursor - 1);
					cursor -= 1;
				}
			}
			KeyCode::Delete => {
				if cursor < input.len() {
					input.remove(cursor);
				}
			}
			KeyCode::Left => cursor = cursor.saturating_sub(1),
			KeyCode::Right => cursor = cursor.min(input.len()),
			KeyCode::Home => cursor = 0,
			KeyCode::End => cursor = input.len(),
			KeyCode::Up => {
				selected_resolution = selected_resolution.saturating_sub(1);
			}
			KeyCode::Down => {
				if selected_resolution < resolutions.len() - 1 {
					selected_resolution += 1;
				}
			}
			KeyCode::Enter => {
				if let Some(error) = validate_aspect_name(&input) {
					self.state = AppState::EnteringAspectName { input, cursor_position: cursor, validation_error: Some(error), selected_resolution };
					return Ok(false);
				}
				let resolution = resolutions[selected_resolution];
				self.start_creating_aspect(input.clone(), resolution);
				self.state = AppState::CreatingAspect { name: input, resolution };
				return Ok(false);
			}
			KeyCode::Esc => {
				self.state = AppState::SelectingAspect;
				return Ok(false);
			}
			_ => {}
		}

		self.state = AppState::EnteringAspectName { input, cursor_position: cursor, validation_error: None, selected_resolution };

		Ok(false)
	}

	async fn handle_creating_aspect(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
			if let Some(cancel) = &self.loading_cancel {
				cancel.store(true, Ordering::SeqCst);
			}
			self.loading_receiver = None;
			self.loading_cancel = None;
			self.state = AppState::SelectingAspect;
		}
		Ok(false)
	}

	async fn handle_csv_path_input(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		let (mut input, mut cursor, _) = match &self.state {
			AppState::EnteringCsvPath { input, cursor_position, validation_error } => (input.clone(), *cursor_position, validation_error.clone()),
			_ => return Ok(false),
		};

		match key.code {
			KeyCode::Char(c) => {
				if !c.is_control() {
					input.insert(cursor, c);
					cursor += 1;
				}
			}
			KeyCode::Backspace => {
				if cursor > 0 {
					input.remove(cursor - 1);
					cursor -= 1;
				}
			}
			KeyCode::Delete => {
				if cursor < input.len() {
					input.remove(cursor);
				}
			}
			KeyCode::Left => cursor = cursor.saturating_sub(1),
			KeyCode::Right => cursor = cursor.min(input.len()),
			KeyCode::Home => cursor = 0,
			KeyCode::End => cursor = input.len(),
			KeyCode::Enter => {
				debug!("Enter pressed for CSV path: '{}'", input);
				if let Some(error) = validate_csv_path(&input) {
					warn!("CSV path validation failed: {}", error);
					self.state = AppState::EnteringCsvPath { input, cursor_position: cursor, validation_error: Some(error) };
					return Ok(false);
				}
				// Strip quotes from path (handles Windows clipboard paste with quotes)
				let clean_path = input.trim().trim_matches('"').trim_matches('\'').to_string();
				info!("Valid CSV path, starting import");
				self.csv_import_progress = Some((0, 0, "Starting CSV import...".to_string()));
				self.start_importing_csv(clean_path);
				self.state = AppState::Plotting;
				return Ok(false);
			}
			KeyCode::Esc => {
				self.state = AppState::Plotting;
				return Ok(false);
			}
			_ => {}
		}

		self.state = AppState::EnteringCsvPath { input, cursor_position: cursor, validation_error: None };

		Ok(false)
	}

	fn start_creating_subject(&mut self, subject_name: String) {
		debug!("Starting subject creation: {}", subject_name);

		let (tx, rx) = mpsc::channel(10);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel);

		let db_name = match self.selected_database.clone() {
			Some(name) => {
				debug!("Using database: {}", name);
				name
			}
			None => {
				error!("No database selected when creating subject");
				drop(tokio::spawn(async move {
					let _ = tx.send(LoadingMessage::Error("No database selected".to_string())).await;
				}));
				return;
			}
		};

		tokio::spawn(async move {
			debug!("Opening database: {}", db_name);
			match Database::existing(&db_name).await {
				Ok(db) => {
					info!("Database opened successfully: {}", db_name);
					debug!("Creating subject: {}", subject_name);
					match db.observe_subject(&subject_name).await {
						Ok(_subject) => {
							info!("Subject created successfully: {}", subject_name);
							debug!("Reloading subject list");
							match list_subjects(&db_name).await {
								Ok(subjects) => {
									debug!("Subject list reloaded with {} subjects", subjects.len());
									let _ = tx.send(LoadingMessage::Subjects(subjects)).await;
								}
								Err(e) => {
									error!("Failed to reload subjects: {}", e);
									let _ = tx.send(LoadingMessage::Error(format!("Failed to reload subjects: {}", e))).await;
								}
							}
						}
						Err(e) => {
							error!("Failed to create subject '{}': {}", subject_name, e);
							let _ = tx.send(LoadingMessage::Error(format!("Failed to create subject: {}", e))).await;
						}
					}
				}
				Err(e) => {
					error!("Failed to open database '{}': {}", db_name, e);
					let _ = tx.send(LoadingMessage::Error(format!("Failed to open database: {}", e))).await;
				}
			}
		});
	}

	fn start_creating_aspect(&mut self, aspect_name: String, resolution: Resolution) {
		let (tx, rx) = mpsc::channel(10);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel);

		let db_name = self.selected_database.clone().unwrap();
		let subject = self.selected_subject.clone().unwrap();

		tokio::spawn(async move {
			match Database::existing(&db_name).await {
				Ok(db) => match db.track_aspect(&subject.id(), &aspect_name, &resolution, None).await {
					Ok(_aspect) => match list_aspects(&db_name, &subject).await {
						Ok(aspects) => {
							let _ = tx.send(LoadingMessage::Aspects(aspects)).await;
						}
						Err(e) => {
							let _ = tx.send(LoadingMessage::Error(format!("Failed to reload aspects: {}", e))).await;
						}
					},
					Err(e) => {
						let _ = tx.send(LoadingMessage::Error(format!("Failed to create aspect: {}", e))).await;
					}
				},
				Err(e) => {
					let _ = tx.send(LoadingMessage::Error(format!("Failed to open database: {}", e))).await;
				}
			}
		});
	}

	async fn handle_database_selection(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}
		match key.code {
			KeyCode::Down => {
				let i = self.list_state.selected().unwrap_or(0);
				if i < self.databases.len() - 1 {
					self.list_state.select(Some(i + 1));
				}
			}
			KeyCode::Up => {
				let i = self.list_state.selected().unwrap_or(0);
				if i > 0 {
					self.list_state.select(Some(i - 1));
				}
			}
			KeyCode::Enter => {
				if let Some(i) = self.list_state.selected() {
					self.selected_database = Some(self.databases[i].clone());
					// Spawn background loading of subjects
					self.start_loading_subjects(self.databases[i].clone());
					self.state = AppState::Loading { loaded_count: 0, total_scanned: 0, status: "Initializing...".to_string() };
				}
			}
			KeyCode::Esc => {
				// Go back to mode selection
				self.state = AppState::SelectingMode;
				self.mode_list_state.select(Some(1)); // Select "Load Existing"
			}
			_ => {}
		}
		Ok(false)
	}

	async fn handle_subject_selection(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		if self.subjects.is_empty() {
			match key.code {
				KeyCode::Char('n') | KeyCode::Char('N') => {
					self.state = AppState::EnteringSubjectName { input: String::new(), cursor_position: 0, validation_error: None };
				}
				KeyCode::Esc => {
					self.state = AppState::SelectingMode;
					self.mode_list_state.select(Some(1));
				}
				_ => {}
			}
			return Ok(false);
		}

		match key.code {
			KeyCode::Down => {
				let i = self.list_state.selected().unwrap_or(0);
				if i < self.subjects.len() - 1 {
					self.list_state.select(Some(i + 1));
				}
			}
			KeyCode::Up => {
				let i = self.list_state.selected().unwrap_or(0);
				if i > 0 {
					self.list_state.select(Some(i - 1));
				}
			}
			KeyCode::Enter => {
				if let Some(i) = self.list_state.selected() {
					self.selected_subject = Some(self.subjects[i].clone());
					let db_name = self.selected_database.as_ref().unwrap().clone();
					let subject = self.subjects[i].clone();
					// Spawn background loading of aspects
					self.start_loading_aspects(db_name, subject);
					self.state = AppState::Loading { loaded_count: 0, total_scanned: 0, status: "Initializing...".to_string() };
				}
			}
			KeyCode::Esc => {
				self.state = AppState::SelectingDatabase;
				self.list_state.select(Some(0));
			}
			_ => {}
		}
		Ok(false)
	}

	async fn handle_aspect_selection(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		if self.aspects.is_empty() {
			match key.code {
				KeyCode::Char('n') | KeyCode::Char('N') => {
					self.state = AppState::EnteringAspectName { input: String::new(), cursor_position: 0, validation_error: None, selected_resolution: 0 };
				}
				KeyCode::Esc => {
					self.state = AppState::SelectingSubject;
					self.list_state.select(Some(0));
				}
				_ => {}
			}
			return Ok(false);
		}

		match key.code {
			KeyCode::Down => {
				let i = self.list_state.selected().unwrap_or(0);
				if i < self.aspects.len() - 1 {
					self.list_state.select(Some(i + 1));
				}
			}
			KeyCode::Up => {
				let i = self.list_state.selected().unwrap_or(0);
				if i > 0 {
					self.list_state.select(Some(i - 1));
				}
			}
			KeyCode::Enter => {
				if let Some(i) = self.list_state.selected() {
					self.selected_aspect = Some(self.aspects[i].clone());
					// Spawn background loading of measurements
					self.start_loading_measurements();
					self.state = AppState::Loading { loaded_count: 0, total_scanned: 0, status: "Initializing...".to_string() };
				}
			}
			KeyCode::Esc => {
				self.state = AppState::SelectingSubject;
				self.list_state.select(Some(0));
			}
			_ => {}
		}
		Ok(false)
	}

	async fn handle_plotting(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}
		match key.code {
			KeyCode::Char('+') => {
				// Zoom in: go to finer resolution (if not already at aspect's native resolution)
				if let Some(aspect) = &self.selected_aspect {
					let native_resolution = aspect.resolution();
					if let Some(new_res) = self.get_finer_resolution(native_resolution) {
						self.view_resolution = new_res;
						self.start_loading_measurements();
						self.state = AppState::Loading { loaded_count: 0, total_scanned: 0, status: "Initializing...".to_string() };
					}
				}
			}
			KeyCode::Char('-') => {
				// Zoom out: go to coarser resolution (up to Years)
				if let Some(new_res) = self.get_coarser_resolution() {
					self.view_resolution = new_res;
					self.start_loading_measurements();
					self.state = AppState::Loading { loaded_count: 0, total_scanned: 0, status: "Initializing...".to_string() };
				}
			}
			KeyCode::Char('c') | KeyCode::Char('C') => {
				// Enter compression mode selection
				debug!("Compression key pressed");
				self.compression_list_state.select(Some(0));
				self.state = AppState::SelectingCompressionMode { selected_index: 0 };
			}
			KeyCode::Char('i') | KeyCode::Char('I') => {
				debug!("Import CSV key pressed");
				self.state = AppState::EnteringCsvPath { input: String::new(), cursor_position: 0, validation_error: None };
			}
			KeyCode::Esc | KeyCode::Char('b') | KeyCode::Char('B') => {
				// If compression pane is showing (complete or error), dismiss it
				if matches!(self.compression_status, Some(CompressionStatus::Complete { .. }) | Some(CompressionStatus::Error(_))) {
					self.dismiss_compression_pane();
				} else if self.compression_status.is_some() {
					// If compression is running, cancel it
					if let Some(cancel) = &self.compression_cancel {
						cancel.store(true, Ordering::SeqCst);
					}
					self.dismiss_compression_pane();
				} else if self.csv_import_progress.is_some() {
					// If CSV import is in progress, cancel it
					if let Some(cancel) = &self.loading_cancel {
						cancel.store(true, Ordering::SeqCst);
					}
					self.csv_import_progress = None;
					self.loading_receiver = None;
					self.loading_cancel = None;
				} else {
					// Otherwise, go back to database selection and refresh the list
					self.refresh_database_list().await;
					self.state = AppState::SelectingDatabase;
					self.selected_database = None;
					self.selected_subject = None;
					self.selected_aspect = None;
					self.subjects.clear();
					self.aspects.clear();
					self.list_state.select(Some(0));
				}
			}
			_ => {}
		}
		Ok(false)
	}

	/// Handle compression mode selection screen
	async fn handle_compression_mode_selection(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		let selected_index = match &self.state {
			AppState::SelectingCompressionMode { selected_index } => *selected_index,
			_ => return Ok(false),
		};

		const NUM_OPTIONS: usize = 7;

		match key.code {
			KeyCode::Down => {
				let new_index = (selected_index + 1).min(NUM_OPTIONS - 1);
				self.state = AppState::SelectingCompressionMode { selected_index: new_index };
				self.compression_list_state.select(Some(new_index));
			}
			KeyCode::Up => {
				let new_index = selected_index.saturating_sub(1);
				self.state = AppState::SelectingCompressionMode { selected_index: new_index };
				self.compression_list_state.select(Some(new_index));
			}
			KeyCode::Enter => {
				match selected_index {
					0 => {
						// Disabled - remove compression config and return
						self.apply_compression_preset_disabled();
						self.state = AppState::Plotting;
					}
					1 => {
						// Time-based (7 years) - apply preset and run
						self.apply_compression_preset_time_7years();
						self.state = AppState::Plotting;
					}
					2 => {
						// Time-based (custom) - go to config screen
						self.state = AppState::ConfiguringCompression {
							mode: CompressionMode::TimeBased,
							time_pure_days: "2555".to_string(), // ~7 years
							time_tier_days: "365".to_string(),
							time_max_tiers: "10".to_string(),
							time_scaling_index: 1, // Exponential
							size_target_gb: "10".to_string(),
							size_min_agg: "0.1".to_string(),
							size_max_agg: "0.95".to_string(),
							base_resolution_index: 6, // Days
							focused_field: CompressionField::TimePureDays,
							validation_error: None,
						};
					}
					3 => {
						// Size-based (10 GB) - apply preset and run
						self.apply_compression_preset_size_10gb();
						self.state = AppState::Plotting;
					}
					4 => {
						// Size-based (1 GB) - apply preset and run
						self.apply_compression_preset_size_1gb();
						self.state = AppState::Plotting;
					}
					5 => {
						// Size-based (custom) - go to config screen
						self.state = AppState::ConfiguringCompression { mode: CompressionMode::SizeBased, time_pure_days: "2555".to_string(), time_tier_days: "365".to_string(), time_max_tiers: "10".to_string(), time_scaling_index: 1, size_target_gb: "10".to_string(), size_min_agg: "0.1".to_string(), size_max_agg: "0.95".to_string(), base_resolution_index: 6, focused_field: CompressionField::SizeTargetGb, validation_error: None };
					}
					6 => {
						// Combined - go to config screen
						self.state = AppState::ConfiguringCompression { mode: CompressionMode::Combined, time_pure_days: "2555".to_string(), time_tier_days: "365".to_string(), time_max_tiers: "10".to_string(), time_scaling_index: 1, size_target_gb: "10".to_string(), size_min_agg: "0.1".to_string(), size_max_agg: "0.95".to_string(), base_resolution_index: 6, focused_field: CompressionField::TimePureDays, validation_error: None };
					}
					_ => {}
				}
			}
			KeyCode::Esc => {
				self.state = AppState::Plotting;
			}
			_ => {}
		}
		Ok(false)
	}

	/// Handle compression configuration screen
	async fn handle_compression_config(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
		if key.kind != KeyEventKind::Press {
			return Ok(false);
		}

		// Extract current state
		let (mode, mut time_pure_days, mut time_tier_days, mut time_max_tiers, mut time_scaling_index, mut size_target_gb, mut size_min_agg, mut size_max_agg, mut base_resolution_index, mut focused_field) = match &self.state {
			AppState::ConfiguringCompression { mode, time_pure_days, time_tier_days, time_max_tiers, time_scaling_index, size_target_gb, size_min_agg, size_max_agg, base_resolution_index, focused_field, .. } => (mode.clone(), time_pure_days.clone(), time_tier_days.clone(), time_max_tiers.clone(), *time_scaling_index, size_target_gb.clone(), size_min_agg.clone(), size_max_agg.clone(), *base_resolution_index, focused_field.clone()),
			_ => return Ok(false),
		};

		// Get list of available fields based on mode
		let fields = self.get_compression_fields_for_mode(&mode);

		match key.code {
			KeyCode::Up => {
				// Move to previous field
				if let Some(idx) = fields.iter().position(|f| *f == focused_field) {
					if idx > 0 {
						focused_field = fields[idx - 1].clone();
					}
				}
			}
			KeyCode::Down => {
				// Move to next field
				if let Some(idx) = fields.iter().position(|f| *f == focused_field) {
					if idx < fields.len() - 1 {
						focused_field = fields[idx + 1].clone();
					}
				}
			}
			KeyCode::Left | KeyCode::Right => {
				// Cycle dropdown values
				match focused_field {
					CompressionField::TimeScaling => {
						time_scaling_index = if key.code == KeyCode::Right {
							(time_scaling_index + 1) % 2
						} else {
							if time_scaling_index == 0 {
								1
							} else {
								0
							}
						};
					}
					CompressionField::BaseResolution => {
						// 0=Nanoseconds, ..., 9=Years
						base_resolution_index = if key.code == KeyCode::Right { (base_resolution_index + 1).min(9) } else { base_resolution_index.saturating_sub(1) };
					}
					_ => {}
				}
			}
			KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
				// Add character to current text field
				match focused_field {
					CompressionField::TimePureDays => time_pure_days.push(c),
					CompressionField::TimeTierDays => time_tier_days.push(c),
					CompressionField::TimeMaxTiers => time_max_tiers.push(c),
					CompressionField::SizeTargetGb => size_target_gb.push(c),
					CompressionField::SizeMinAgg => size_min_agg.push(c),
					CompressionField::SizeMaxAgg => size_max_agg.push(c),
					_ => {}
				}
			}
			KeyCode::Backspace => {
				// Remove character from current text field
				match focused_field {
					CompressionField::TimePureDays => {
						time_pure_days.pop();
					}
					CompressionField::TimeTierDays => {
						time_tier_days.pop();
					}
					CompressionField::TimeMaxTiers => {
						time_max_tiers.pop();
					}
					CompressionField::SizeTargetGb => {
						size_target_gb.pop();
					}
					CompressionField::SizeMinAgg => {
						size_min_agg.pop();
					}
					CompressionField::SizeMaxAgg => {
						size_max_agg.pop();
					}
					_ => {}
				}
			}
			KeyCode::Enter => {
				// Validate and run compression
				match self.build_and_run_compression(&mode, &time_pure_days, &time_tier_days, &time_max_tiers, time_scaling_index, &size_target_gb, &size_min_agg, &size_max_agg, base_resolution_index) {
					Ok(()) => {
						self.state = AppState::Plotting;
						return Ok(false);
					}
					Err(e) => {
						// Show validation error
						self.state = AppState::ConfiguringCompression { mode, time_pure_days, time_tier_days, time_max_tiers, time_scaling_index, size_target_gb, size_min_agg, size_max_agg, base_resolution_index, focused_field, validation_error: Some(e.to_string()) };
						return Ok(false);
					}
				}
			}
			KeyCode::Esc => {
				// Go back to mode selection
				self.state = AppState::SelectingCompressionMode { selected_index: 0 };
				self.compression_list_state.select(Some(0));
			}
			_ => {}
		}

		// Update state with new values
		self.state = AppState::ConfiguringCompression { mode, time_pure_days, time_tier_days, time_max_tiers, time_scaling_index, size_target_gb, size_min_agg, size_max_agg, base_resolution_index, focused_field, validation_error: None };

		Ok(false)
	}

	/// Get list of fields available for the given compression mode
	fn get_compression_fields_for_mode(&self, mode: &CompressionMode) -> Vec<CompressionField> {
		match mode {
			CompressionMode::TimeBased => vec![CompressionField::TimePureDays, CompressionField::TimeTierDays, CompressionField::TimeMaxTiers, CompressionField::TimeScaling, CompressionField::BaseResolution],
			CompressionMode::SizeBased => vec![CompressionField::SizeTargetGb, CompressionField::SizeMinAgg, CompressionField::SizeMaxAgg, CompressionField::BaseResolution],
			CompressionMode::Combined => vec![CompressionField::TimePureDays, CompressionField::TimeTierDays, CompressionField::TimeMaxTiers, CompressionField::TimeScaling, CompressionField::SizeTargetGb, CompressionField::SizeMinAgg, CompressionField::SizeMaxAgg, CompressionField::BaseResolution],
		}
	}

	/// Apply "Disabled" preset - removes compression config in background
	fn apply_compression_preset_disabled(&mut self) {
		// Disable compression in background to avoid blocking UI
		let db_name = self.selected_database.clone().unwrap();
		let aspect = self.selected_aspect.clone().unwrap();

		self.compression_status = Some(CompressionStatus::Running { phase: "Disabling compression...".to_string(), progress_percent: None, current_tier: None, total_tiers: None, aggressiveness: None, time_range: None });

		let (tx, rx) = mpsc::channel(10);
		self.compression_receiver = Some(rx);

		tokio::spawn(async move {
			match Database::existing(&db_name).await {
				Ok(_db) => {
					let mut aspect = aspect;
					// Set locally first (instant)
					aspect.set_compression_config_local(None);

					// Try to persist - if it fails, warn but still report success
					if let Err(e) = aspect.set_compression_config(None).await {
						warn!("Failed to persist compression config removal: {}", e);
					}

					let _ = tx.send(CompressionMessage::Complete { original_count: 0, compressed_count: 0, compression_ratio: 0.0, final_size_bytes: 0, time_based_tiers: 0, size_based_iterations: 0, duration_ms: 0 }).await;
				}
				Err(e) => {
					let _ = tx.send(CompressionMessage::Error(format!("Failed to open database: {}", e))).await;
				}
			}
		});
	}

	/// Apply "Time-based (7 years)" preset and run compression
	fn apply_compression_preset_time_7years(&mut self) {
		let config = CompressionConfig::time_based(TimeBasedCompressionConfig::default_seven_years());
		self.apply_compression_config_and_run(config);
	}

	/// Apply "Size-based (10 GB)" preset and run compression
	fn apply_compression_preset_size_10gb(&mut self) {
		let config = CompressionConfig::size_based(SizeBasedCompressionConfig::default_10gb());
		self.apply_compression_config_and_run(config);
	}

	/// Apply "Size-based (1 GB)" preset and run compression
	fn apply_compression_preset_size_1gb(&mut self) {
		let config = CompressionConfig::size_based(SizeBasedCompressionConfig::default_1gb());
		self.apply_compression_config_and_run(config);
	}

	/// Apply compression config to aspect and start compression
	fn apply_compression_config_and_run(&mut self, config: CompressionConfig) {
		// Start compression in background (config will be set there to avoid blocking UI)
		self.start_compression_with_config(config);
		self.state = AppState::Plotting;
	}

	/// Build compression config from form fields and run compression
	#[allow(clippy::too_many_arguments)] // Form processing requires all these fields
	fn build_and_run_compression(&mut self, mode: &CompressionMode, time_pure_days: &str, time_tier_days: &str, time_max_tiers: &str, time_scaling_index: usize, size_target_gb: &str, size_min_agg: &str, size_max_agg: &str, base_resolution_index: usize) -> Result<()> {
		// Parse and validate fields
		let base_resolution = RESOLUTION_ORDER[base_resolution_index];

		let config = match mode {
			CompressionMode::TimeBased => {
				let pure_days: i64 = time_pure_days.parse().map_err(|_| anyhow!("Invalid pure duration"))?;
				let tier_days: i64 = time_tier_days.parse().map_err(|_| anyhow!("Invalid tier duration"))?;
				let max_tiers: u32 = time_max_tiers.parse().map_err(|_| anyhow!("Invalid max tiers"))?;
				let scaling = if time_scaling_index == 0 { AggressivenessScaling::Linear } else { AggressivenessScaling::Exponential { base: 0.5 } };

				let time_config = TimeBasedCompressionConfig::new(Duration::days(pure_days), Duration::days(tier_days)).with_max_tiers(max_tiers).with_scaling(scaling);

				CompressionConfig::time_based(time_config).with_base_resolution(base_resolution)
			}
			CompressionMode::SizeBased => {
				let target_gb: f64 = size_target_gb.parse().map_err(|_| anyhow!("Invalid target size"))?;
				let min_agg: f64 = size_min_agg.parse().map_err(|_| anyhow!("Invalid min aggressiveness"))?;
				let max_agg: f64 = size_max_agg.parse().map_err(|_| anyhow!("Invalid max aggressiveness"))?;

				if !(0.0..=1.0).contains(&min_agg) || !(0.0..=1.0).contains(&max_agg) {
					return Err(anyhow!("Aggressiveness must be between 0 and 1"));
				}

				let target_bytes = (target_gb * 1024.0 * 1024.0 * 1024.0) as u64;
				let mut size_config = SizeBasedCompressionConfig::new(target_bytes);
				size_config.min_aggressiveness = min_agg;
				size_config.max_aggressiveness = max_agg;

				CompressionConfig::size_based(size_config).with_base_resolution(base_resolution)
			}
			CompressionMode::Combined => {
				let pure_days: i64 = time_pure_days.parse().map_err(|_| anyhow!("Invalid pure duration"))?;
				let tier_days: i64 = time_tier_days.parse().map_err(|_| anyhow!("Invalid tier duration"))?;
				let max_tiers: u32 = time_max_tiers.parse().map_err(|_| anyhow!("Invalid max tiers"))?;
				let scaling = if time_scaling_index == 0 { AggressivenessScaling::Linear } else { AggressivenessScaling::Exponential { base: 0.5 } };

				let time_config = TimeBasedCompressionConfig::new(Duration::days(pure_days), Duration::days(tier_days)).with_max_tiers(max_tiers).with_scaling(scaling);

				let target_gb: f64 = size_target_gb.parse().map_err(|_| anyhow!("Invalid target size"))?;
				let min_agg: f64 = size_min_agg.parse().map_err(|_| anyhow!("Invalid min aggressiveness"))?;
				let max_agg: f64 = size_max_agg.parse().map_err(|_| anyhow!("Invalid max aggressiveness"))?;

				if !(0.0..=1.0).contains(&min_agg) || !(0.0..=1.0).contains(&max_agg) {
					return Err(anyhow!("Aggressiveness must be between 0 and 1"));
				}

				let target_bytes = (target_gb * 1024.0 * 1024.0 * 1024.0) as u64;
				let mut size_config = SizeBasedCompressionConfig::new(target_bytes);
				size_config.min_aggressiveness = min_agg;
				size_config.max_aggressiveness = max_agg;

				CompressionConfig::combined(time_config, size_config).with_base_resolution(base_resolution)
			}
		};

		self.apply_compression_config_and_run(config);
		Ok(())
	}

	/// Start background compression task with a new config
	fn start_compression_with_config(&mut self, config: CompressionConfig) {
		let (tx, rx) = mpsc::channel(100);
		let cancel = Arc::new(AtomicBool::new(false));
		self.compression_receiver = Some(rx);
		self.compression_cancel = Some(cancel.clone());
		self.compression_status = Some(CompressionStatus::Running { phase: "Starting compression...".to_string(), progress_percent: None, current_tier: None, total_tiers: None, aggressiveness: None, time_range: None });

		let db_name = self.selected_database.clone().unwrap();
		let aspect = self.selected_aspect.clone().unwrap();

		tokio::spawn(async move {
			// Open database and run compression
			match Database::existing(&db_name).await {
				Ok(db) => {
					let mut aspect = aspect;

					// Persist compression config BEFORE starting compression
					// This avoids database connection issues after long compression runs
					if let Err(e) = aspect.set_compression_config(Some(config.clone())).await {
						warn!("Failed to persist compression config: {}", e);
						// Continue anyway - compression can still run with local config
					}

					let _ = tx.send(CompressionMessage::Progress { phase: "Initializing compression...".to_string(), progress_percent: Some(0), current_tier: None, total_tiers: None, aggressiveness: None, time_range: None }).await;

					// Create progress callback to send updates to the TUI
					let tx_progress = tx.clone();
					let progress_callback: weftdb::ProgressCallback = std::sync::Arc::new(move |progress| {
						let phase_str = format!("{}", progress.phase);
						let progress_percent = if progress.total_tiers > 0 { Some(((progress.current_tier as f64 / progress.total_tiers as f64) * 100.0) as u8) } else { None };
						let time_range = if progress.time_range_start != progress.time_range_end { Some((progress.time_range_start, progress.time_range_end)) } else { None };
						// Use blocking send since we're in a sync callback
						let _ = tx_progress.try_send(CompressionMessage::Progress { phase: phase_str, progress_percent, current_tier: Some(progress.current_tier), total_tiers: Some(progress.total_tiers), aggressiveness: Some(progress.aggressiveness), time_range });
					});

					info!(">>> Starting compress_with_progress...");
					let compression_start = std::time::Instant::now();
					let compression_result = aspect.compress_with_progress(&db, Some(progress_callback)).await;
					info!(">>> compress_with_progress returned");
					match compression_result {
						Ok(detailed_summary) => {
							let duration_ms = compression_start.elapsed().as_millis() as u64;
							info!(">>> Compression returned Ok, duration_ms={}", duration_ms);

							// Config was already persisted before compression started
							// No need to persist again after completion

							// Save compression history to database (ensure tables exist for older DBs)
							info!("Ensuring compression tables exist...");
							if let Err(e) = aspect.ensure_compression_tables().await {
								warn!("Failed to ensure compression tables: {}", e);
							}
							info!("Saving compression history...");
							if let Err(e) = aspect.save_compression_history(&detailed_summary, &config).await {
								warn!("Failed to save compression history: {}", e);
							}
							info!("History saved");

							info!("Sending Complete message...");
							let _ = tx.send(CompressionMessage::Complete { original_count: detailed_summary.summary.total_original_count, compressed_count: detailed_summary.summary.total_compressed_count, compression_ratio: detailed_summary.summary.overall_compression_ratio(), final_size_bytes: detailed_summary.summary.final_size_bytes, time_based_tiers: detailed_summary.summary.time_based_results.len(), size_based_iterations: detailed_summary.summary.size_based_results.len(), duration_ms }).await;
							info!("Complete message sent");
						}
						Err(e) => {
							let _ = tx.send(CompressionMessage::Error(e.to_string())).await;
						}
					}
				}
				Err(e) => {
					let _ = tx.send(CompressionMessage::Error(format!("Failed to open database: {}", e))).await;
				}
			}
		});
	}

	/// Poll compression progress messages
	pub fn poll_compression(&mut self) {
		let mut should_clear = false;
		let mut should_refresh_measurements = false;
		let mut new_status = None;

		if let Some(ref mut rx) = self.compression_receiver {
			while let Ok(msg) = rx.try_recv() {
				match msg {
					CompressionMessage::Progress { phase, progress_percent, current_tier, total_tiers, aggressiveness, time_range } => {
						new_status = Some(CompressionStatus::Running { phase, progress_percent, current_tier, total_tiers, aggressiveness, time_range });
					}
					CompressionMessage::Complete { original_count, compressed_count, compression_ratio, final_size_bytes, time_based_tiers, size_based_iterations, duration_ms } => {
						new_status = Some(CompressionStatus::Complete { original_count, compressed_count, compression_ratio, final_size_bytes, time_based_tiers, size_based_iterations, duration_ms });
						should_clear = true;
						should_refresh_measurements = true;
					}
					CompressionMessage::Error(e) => {
						new_status = Some(CompressionStatus::Error(e));
						should_clear = true;
					}
					CompressionMessage::StatsLoaded { last_compression, dirty_regions_count } => {
						new_status = Some(CompressionStatus::Idle { last_compression, dirty_regions_count });
						should_clear = true;
					}
				}
			}
		}

		if let Some(status) = new_status {
			self.compression_status = Some(status);
		}
		if should_clear {
			self.compression_receiver = None;
			self.compression_cancel = None;
		}
		if should_refresh_measurements {
			self.start_loading_measurements();
		}
	}

	/// Dismiss the compression pane - transitions to Idle state to show last stats
	fn dismiss_compression_pane(&mut self) {
		// Transition to Idle state instead of None so we continue to show the pane
		// with last compression stats
		self.compression_status = Some(CompressionStatus::Idle { last_compression: None, dirty_regions_count: 0 });
		self.compression_receiver = None;
		self.compression_cancel = None;
		// Reload compression stats in background
		self.start_loading_compression_stats();
	}

	/// Start background task to load compression stats for the selected aspect
	fn start_loading_compression_stats(&mut self) {
		let db_name = match self.selected_database.clone() {
			Some(name) => name,
			None => return,
		};
		let aspect = match self.selected_aspect.clone() {
			Some(a) => a,
			None => return,
		};

		let (tx, rx) = mpsc::channel(10);
		self.compression_receiver = Some(rx);

		tokio::spawn(async move {
			match Database::existing(&db_name).await {
				Ok(_db) => {
					let mut aspect = aspect;
					// Ensure compression tables exist for older databases
					if let Err(e) = aspect.ensure_compression_tables().await {
						debug!("Failed to ensure compression tables: {}", e);
					}
					// Get last compression info
					let last_compression = match aspect.get_last_compression().await {
						Ok(Some(info)) => Some(LastCompressionInfo { completed_at: info.completed_at, original_count: info.original_count, compressed_count: info.compressed_count, compression_ratio: info.compression_ratio, time_based_tiers: info.time_based_tiers, duration_ms: info.duration_ms }),
						Ok(None) => None,
						Err(e) => {
							debug!("Failed to load last compression info: {}", e);
							None
						}
					};

					// Get dirty regions count
					let dirty_regions_count = aspect.get_dirty_regions_count().await.unwrap_or(0);

					// Send stats via the compression channel
					let _ = tx.send(CompressionMessage::StatsLoaded { last_compression, dirty_regions_count }).await;
				}
				Err(e) => {
					debug!("Failed to open database for compression stats: {}", e);
				}
			}
		});
	}

	/// Start background measurement loading task
	fn start_loading_measurements(&mut self) {
		let (tx, rx) = mpsc::channel(100);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel.clone());
		self.loading_in_progress = true;

		// Initialize compression status to Idle with placeholder values
		// The actual stats will be loaded when measurements finish loading
		if self.compression_status.is_none() {
			self.compression_status = Some(CompressionStatus::Idle { last_compression: None, dirty_regions_count: 0 });
		}

		// Immediately notify UI that loading has started so spinner/progress updates.
		let _ = tx.try_send(LoadingMessage::Progress { loaded: 0, scanned: 0, status: "Initializing...".to_string() });

		let db_name = match self.selected_database.clone() {
			Some(name) => name,
			None => {
				warn!("start_loading_measurements called with no database selected");
				return;
			}
		};
		let aspect = match self.selected_aspect.as_ref() {
			Some(a) => a,
			None => {
				warn!("start_loading_measurements called with no aspect selected");
				return;
			}
		};
		let aspect_id = aspect.id();
		let native_resolution = aspect.resolution();
		let view_resolution = self.view_resolution;

		tokio::spawn(async move {
			// Brief delay to allow database to stabilize after compression
			tokio::time::sleep(std::time::Duration::from_millis(200)).await;

			let result = load_measurements_background(db_name.clone(), aspect_id, native_resolution, view_resolution, tx.clone(), cancel.clone()).await;
			if let Err(e) = result {
				let _ = tx.send(LoadingMessage::Error(e.to_string())).await;
			}
		});
	}

	/// Start background loading of subjects for a database
	fn start_loading_subjects(&mut self, db_name: String) {
		let (tx, rx) = mpsc::channel(10);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel.clone());

		// Notify UI that loading of subjects has started
		let _ = tx.try_send(LoadingMessage::Progress { loaded: 0, scanned: 0, status: "Initializing...".to_string() });

		tokio::spawn(async move {
			// Attempt to list subjects; send result back
			match list_subjects(&db_name).await {
				Ok(subjects) => {
					let _ = tx.send(LoadingMessage::Subjects(subjects)).await;
				}
				Err(e) => {
					let _ = tx.send(LoadingMessage::Error(e.to_string())).await;
				}
			}
		});
	}

	/// Start background loading of aspects for a subject
	fn start_loading_aspects(&mut self, db_name: String, subject: Subject) {
		let (tx, rx) = mpsc::channel(10);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel.clone());

		// Notify UI that loading of aspects has started
		let _ = tx.try_send(LoadingMessage::Progress { loaded: 0, scanned: 0, status: "Initializing...".to_string() });

		tokio::spawn(async move {
			match list_aspects(&db_name, &subject).await {
				Ok(aspects) => {
					let _ = tx.send(LoadingMessage::Aspects(aspects)).await;
				}
				Err(e) => {
					let _ = tx.send(LoadingMessage::Error(e.to_string())).await;
				}
			}
		});
	}

	/// Start background CSV import task
	fn start_importing_csv(&mut self, file_path: String) {
		debug!("Starting CSV import: {}", file_path);

		let (tx, rx) = mpsc::channel(100);
		let cancel = Arc::new(AtomicBool::new(false));
		self.loading_receiver = Some(rx);
		self.loading_cancel = Some(cancel.clone());

		// Initial progress message
		let _ = tx.try_send(LoadingMessage::Progress { loaded: 0, scanned: 0, status: "Initializing...".to_string() });

		let db_name = self.selected_database.clone().unwrap();
		let subject_id = self.selected_subject.as_ref().unwrap().id();
		let aspect_id = self.selected_aspect.as_ref().unwrap().id();
		let view_resolution = self.view_resolution;

		tokio::spawn(async move {
			match import_csv_background(file_path.clone(), db_name.clone(), subject_id, aspect_id, view_resolution, tx.clone(), cancel.clone()).await {
				Ok(_) => {
					info!("CSV import completed successfully");
				}
				Err(e) => {
					error!("CSV import failed: {}", e);
					let _ = tx.send(LoadingMessage::Error(format!("CSV import failed: {}", e))).await;
				}
			}
		});
	}
}

/// Background task for loading measurements
async fn load_measurements_background(db_name: String, aspect_id: weftdb::AspectId, native_resolution: Resolution, view_resolution: Resolution, tx: mpsc::Sender<LoadingMessage>, cancel: Arc<AtomicBool>) -> Result<()> {
	let db = Database::existing(&db_name).await?;

	// Notify UI that loading has started
	let _ = tx.send(LoadingMessage::Progress { loaded: 0, scanned: 0, status: "Initializing...".to_string() }).await;

	// Get time range of data
	let start = match db.get_earliest_measurement(&aspect_id).await? {
		Some(s) => s,
		None => {
			let _ = tx.send(LoadingMessage::CompleteMeasurements(Vec::new())).await;
			return Ok(());
		}
	};
	let end = match db.get_latest_measurement(&aspect_id).await? {
		Some(e) => e,
		None => {
			let _ = tx.send(LoadingMessage::CompleteMeasurements(Vec::new())).await;
			return Ok(());
		}
	};

	// Always use the user's selected view_resolution
	// Send the resolution we're using to the UI
	let _ = tx.send(LoadingMessage::DefaultResolution(view_resolution)).await;

	info!("Loading measurements with resolution: {:?} (native: {:?})", view_resolution, native_resolution);

	// Use analyze_range for all loads - no point limit
	let method = Spline::Linear;
	let mut point_stream = match Outputs::analyze_range(&db, &aspect_id, start, end, view_resolution, method).await {
		Ok(s) => s,
		Err(e) => {
			warn!("analyze_range failed: {}", e);
			return Err(anyhow!("analyze_range failed: {}", e));
		}
	};

	let mut measurements = Vec::new();
	let mut scanned = 0usize;

	// Adaptive debouncing for incremental plot updates
	let min_update_interval = std::time::Duration::from_millis(300);
	let mut last_update_time = Instant::now();
	let mut last_update_duration = std::time::Duration::from_millis(50);
	let mut points_since_last_update = 0usize;
	const MIN_POINTS_PER_UPDATE: usize = 1000;

	while let Some(r) = point_stream.next().await {
		if cancel.load(Ordering::SeqCst) {
			return Ok(());
		}
		let p = r?;
		scanned += 1;
		let value_f64 = p.value.to_f64().unwrap_or(0.0);

		// Log first few points to help debug zero values issue
		if scanned <= 5 {
			info!("Point {}: timestamp={}, raw_value={}, f64_value={}", scanned, p.timestamp, p.value, value_f64);
		}

		measurements.push((p.timestamp, value_f64));
		points_since_last_update += 1;

		// Yield to event loop periodically to prevent blocking UI updates
		// The stream's .next().await doesn't actually yield when iterating buffered points
		if scanned.is_multiple_of(500) {
			tokio::task::yield_now().await;
		}

		// Adaptive incremental update
		let elapsed = last_update_time.elapsed();
		let adaptive_interval = (last_update_duration * 2).max(min_update_interval);

		if points_since_last_update >= MIN_POINTS_PER_UPDATE && elapsed >= adaptive_interval {
			let update_start = Instant::now();
			let _ = tx.send(LoadingMessage::UpdateMeasurements(measurements.clone())).await;
			let _ = tx.send(LoadingMessage::Progress { loaded: measurements.len(), scanned, status: format!("Streaming at {:?} resolution...", view_resolution) }).await;
			last_update_duration = update_start.elapsed();
			last_update_time = Instant::now();
			points_since_last_update = 0;
		}
	}

	info!("Loaded {} measurements at {:?} resolution", measurements.len(), view_resolution);
	let _ = tx.send(LoadingMessage::CompleteMeasurements(measurements)).await;
	Ok(())
}

fn validate_database_name(name: &str, existing: &[String]) -> Option<String> {
	if name.trim().is_empty() {
		return Some("Database name cannot be empty".to_string());
	}
	if name.len() > 50 {
		return Some("Name too long (max 50 characters)".to_string());
	}
	if name.chars().next().map(|c| c.is_numeric()).unwrap_or(false) {
		return Some("Cannot start with a number".to_string());
	}
	if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
		return Some("Only letters, numbers, underscore, and hyphen allowed".to_string());
	}
	if existing.iter().any(|db| db.eq_ignore_ascii_case(name)) {
		return Some("Database already exists".to_string());
	}

	None
}

fn validate_subject_name(name: &str) -> Option<String> {
	if name.trim().is_empty() {
		return Some("Subject name cannot be empty".to_string());
	}
	if name.len() > 100 {
		return Some("Name too long (max 100 characters)".to_string());
	}
	None
}

fn validate_aspect_name(name: &str) -> Option<String> {
	if name.trim().is_empty() {
		return Some("Aspect name cannot be empty".to_string());
	}
	if name.len() > 100 {
		return Some("Name too long (max 100 characters)".to_string());
	}
	None
}

fn validate_csv_path(path: &str) -> Option<String> {
	let path = path.trim().trim_matches('"').trim_matches('\'');
	if path.is_empty() {
		return Some("File path cannot be empty".to_string());
	}
	let path_obj = std::path::Path::new(path);
	if !path_obj.exists() {
		return Some("File does not exist".to_string());
	}
	if !path_obj.is_file() {
		return Some("Path is not a file".to_string());
	}
	None
}

async fn list_databases() -> Result<Vec<String>> {
	// List directories in the active data dir that contain metadata.db
	use std::fs;
	let data_dir = weftdb::data_dir();
	let mut dbs = vec![];
	let entries = match fs::read_dir(&data_dir) {
		Ok(entries) => entries,
		Err(_) => return Ok(dbs), // Data directory doesn't exist yet — no databases
	};
	for entry in entries {
		let entry = entry?;
		if entry.path().is_dir() {
			let metadata_path = entry.path().join("metadata.db");
			if metadata_path.exists() {
				if let Some(name) = entry.file_name().to_str() {
					dbs.push(name.to_string());
				}
			}
		}
	}
	Ok(dbs)
}

async fn list_subjects(db_name: &str) -> Result<Vec<Subject>> {
	let data_dir = weftdb::data_dir();
	let db_path = format!("{}/{}", data_dir, db_name);
	let metadata_path = format!("{}/metadata.db", db_path);
	if !std::path::Path::new(&metadata_path).exists() {
		return Err(anyhow!("Database metadata file not found at: {}", metadata_path));
	}

	// Debug: Try to read what names are stored in the database table
	let db = match Database::existing(db_name).await {
		Ok(db) => db,
		Err(e) => {
			// If it fails, try to query the metadata.db directly to see what's stored
			return Err(anyhow!(
				"Failed to open database '{}' (folder exists at {}): {}. \
                Check if the database name in metadata.db matches the folder name.",
				db_name,
				db_path,
				e
			));
		}
	};
	let db_info = db.get_database_info().await?;
	Ok(db_info.subjects().values().cloned().collect())
}

async fn list_aspects(db_name: &str, subject: &Subject) -> Result<Vec<Aspect>> {
	let db = Database::existing(db_name).await?;
	db.list_aspects(&subject.id()).await
}

/// Parse timestamp from various formats
fn parse_timestamp(s: &str) -> Result<DateTime<Utc>> {
	let s = s.trim();

	// Try parsing as floating-point or integer (Unix timestamp)
	// First try f64 to handle decimals like 1325412060.0
	if let Ok(num_f64) = s.parse::<f64>() {
		// If it looks like a Unix timestamp (between 1970 and 2286)
		if (1_000_000_000.0..10_000_000_000.0).contains(&num_f64) {
			// Assume seconds with possible fractional part
			let secs = num_f64.floor() as i64;
			let nanos = ((num_f64.fract() * 1_000_000_000.0) as u32).min(999_999_999);
			if let Some(dt) = DateTime::from_timestamp(secs, nanos) {
				return Ok(dt);
			}
		}
		// If 13+ digits, assume milliseconds
		if num_f64 >= 1_000_000_000_000.0 {
			let millis = num_f64 as i64;
			if let Some(dt) = DateTime::from_timestamp_millis(millis) {
				return Ok(dt);
			}
		}
	}

	// Fallback: Try parsing as integer (for cases without decimals)
	if let Ok(num) = s.parse::<i64>() {
		// If 10 digits (~1970-2286), assume seconds
		if (1_000_000_000..10_000_000_000).contains(&num) {
			if let Some(dt) = DateTime::from_timestamp(num, 0) {
				return Ok(dt);
			}
		}
		// If 13+ digits, assume milliseconds
		if num >= 1_000_000_000_000 {
			if let Some(dt) = DateTime::from_timestamp_millis(num) {
				return Ok(dt);
			}
		}
	}

	// Try RFC3339/ISO8601
	if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
		return Ok(dt.with_timezone(&Utc));
	}

	// Try RFC2822
	if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
		return Ok(dt.with_timezone(&Utc));
	}

	// Try common formats
	let formats = ["%Y-%m-%d %H:%M:%S", "%Y/%m/%d %H:%M:%S", "%m-%d-%Y %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%d/%m/%Y %H:%M:%S", "%Y-%m-%d", "%Y/%m/%d"];

	for format in &formats {
		if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, format) {
			let aware = DateTime::from_naive_utc_and_offset(dt, Utc);
			return Ok(aware);
		}
	}

	Err(anyhow!("Could not parse timestamp: {}", s))
}

/// Background task for importing CSV measurements
/// Debounces UI updates to prevent excessive redrawing on large files
async fn import_csv_background(file_path: String, db_name: String, _subject_id: weftdb::SubjectId, aspect_id: weftdb::AspectId, view_resolution: Resolution, tx: mpsc::Sender<LoadingMessage>, cancel: Arc<AtomicBool>) -> Result<()> {
	use std::{str::FromStr, time::Instant};

	use bigdecimal::BigDecimal;

	info!("CSV import started: {}", file_path);

	// Read file as string to handle quote formatting issues (use async I/O)
	let file_content = tokio::fs::read_to_string(&file_path).await?;
	let lines: Vec<&str> = file_content.lines().collect();

	if lines.is_empty() {
		return Err(anyhow!("CSV file is empty"));
	}

	// Parse header line - remove quotes and split by comma
	let header_line = lines[0].trim().trim_matches('"').to_lowercase();

	debug!("CSV header line (cleaned): '{}'", header_line);

	let header_parts: Vec<&str> = header_line.split(',').map(|s| s.trim()).collect();
	debug!("CSV headers: {:?}", header_parts);

	let mut timestamp_idx = None;
	let mut value_idx = None;

	for (i, header) in header_parts.iter().enumerate() {
		if header.contains("timestamp") || header.contains("time") {
			timestamp_idx = Some(i);
			info!("Found timestamp column at index {}", i);
		}
		if header.contains("value") || header.contains("val") {
			value_idx = Some(i);
			info!("Found value column at index {}", i);
		}
	}

	let timestamp_idx = timestamp_idx.ok_or_else(|| anyhow!("No 'timestamp' column found in headers: {:?}", header_parts))?;
	let value_idx = value_idx.ok_or_else(|| anyhow!("No 'value' column found in headers: {:?}", header_parts))?;

	info!("CSV columns: timestamp at {}, value at {}", timestamp_idx, value_idx);

	// Send initial progress message
	let total_rows = lines.len().saturating_sub(1); // Subtract header
	let _ = tx.send(LoadingMessage::Progress { loaded: 0, scanned: total_rows, status: format!("Parsing CSV ({} rows)...", total_rows) }).await;

	// Parse measurements from CSV
	let mut measurements = Vec::new();
	let mut error_count = 0u32;
	let mut row_num = 1usize;
	let mut last_progress_update = 0usize;

	for line in &lines[1..] {
		if cancel.load(Ordering::SeqCst) {
			info!("CSV import cancelled");
			return Ok(());
		}

		row_num += 1;
		let clean_line = line.trim().trim_matches('"');

		// Split by comma
		let fields: Vec<&str> = clean_line.split(',').map(|s| s.trim().trim_matches('"')).collect();

		if fields.len() < value_idx.max(timestamp_idx) + 1 {
			error_count += 1;
			if error_count <= 10 {
				debug!("Row {}: Not enough fields (expected at least {}): {:?}", row_num, value_idx.max(timestamp_idx) + 1, fields);
			}
			continue;
		}

		// Extract timestamp and value
		let timestamp_str = fields.get(timestamp_idx).unwrap_or(&"").trim();
		let value_str = fields.get(value_idx).unwrap_or(&"").trim();

		// Try to parse timestamp
		let timestamp = match parse_timestamp(timestamp_str) {
			Ok(ts) => ts,
			Err(e) => {
				error_count += 1;
				if error_count <= 10 {
					debug!("Row {}: Failed to parse timestamp '{}': {}", row_num, timestamp_str, e);
				}
				continue;
			}
		};

		// Try to parse value
		let value = match value_str.parse::<f64>() {
			Ok(v) => v,
			Err(e) => {
				error_count += 1;
				if error_count <= 10 {
					debug!("Row {}: Failed to parse value '{}': {}", row_num, value_str, e);
				}
				continue;
			}
		};

		// Create InputMeasurement
		let bd_value = BigDecimal::from_str(&value.to_string())?;
		let measurement = InputMeasurement::new(timestamp, bd_value);
		measurements.push(measurement);

		// Send progress update every 5000 rows and yield to let other tasks run
		if row_num - last_progress_update >= 5000 || row_num == lines.len() {
			let percent = if total_rows > 0 { ((row_num as f64 / total_rows as f64) * 100.0) as u32 } else { 0 };
			let _ = tx
				.send(LoadingMessage::Progress {
					loaded: row_num,     // Current row position (for progress bar)
					scanned: total_rows, // Total rows (for progress bar)
					status: format!("Parsing CSV: {}% ({}/{} rows, {} parsed)", percent, row_num, total_rows, measurements.len()),
				})
				.await;
			last_progress_update = row_num;
			// Yield to allow UI and other tasks to run
			tokio::task::yield_now().await;
		}
	}

	info!("Parsed {} measurements from CSV (skipped {} errors)", measurements.len(), error_count);

	if measurements.is_empty() {
		return Err(anyhow!("No valid measurements parsed from CSV"));
	}

	// Send progress update after parsing complete
	let _ = tx.send(LoadingMessage::Progress { loaded: measurements.len(), scanned: measurements.len(), status: format!("CSV parsed successfully! Importing {} measurements...", measurements.len()) }).await;

	// Import measurements in batches
	let db = Database::existing(&db_name).await?;
	let chunk_size = if measurements.len() > 500_000 { 500_000 } else { 250_000 };
	let mut last_progress_update = Instant::now();
	let progress_interval = std::time::Duration::from_millis(500); // Update progress bar every 500ms
	let dataset_id = DatasetId::new();

	info!("Starting import of {} measurements in chunks of {} (dataset_id: {:?})", measurements.len(), chunk_size, dataset_id);

	// Spawn a separate task to update plot during import
	// Uses adaptive debouncing: waits 2x the duration of the last update before starting the next
	// This naturally slows down when updates are expensive (large datasets)
	let db_name_clone = db_name.clone();
	let aspect_id_clone = aspect_id;
	let tx_clone = tx.clone();
	let cancel_clone = cancel.clone();
	let plot_task = tokio::spawn(async move {
		let min_interval = std::time::Duration::from_secs(2);
		let mut last_update_duration = std::time::Duration::from_secs(1);
		let import_start = std::time::Instant::now();
		// Skip plot updates for first 60 seconds to avoid contention with critical import chunks
		let plot_update_start_delay = std::time::Duration::from_secs(60);

		loop {
			// Wait for 2x the last update duration (adaptive debounce), minimum 2 seconds
			let wait_time = (last_update_duration * 2).max(min_interval);
			debug!("Plot update: waiting {:?} before next update (last took {:?})", wait_time, last_update_duration);
			tokio::time::sleep(wait_time).await;

			if cancel_clone.load(Ordering::SeqCst) {
				break;
			}

			// Skip plot updates for first 60 seconds of import to prevent contention
			// This allows critical chunks to be imported quickly without lock check overhead
			if import_start.elapsed() < plot_update_start_delay {
				debug!("[plot_update] Deferring plot updates until first minute of import completes");
				continue;
			}

			// Perform the plot update and measure how long it takes
			let update_start = std::time::Instant::now();

			if let Ok(plot_db) = Database::existing(&db_name_clone).await {
				// Check if measurement database is locked before attempting query
				// This is more accurate than checking metadata db since analyze_range queries the measurement DB
				if plot_db.is_measurement_db_locked(&aspect_id_clone).await {
					debug!("[plot_update] Measurement database locked, skipping this iteration");
					let _ = tx_clone.send(LoadingMessage::DatabaseSkipped).await;
					continue;
				}

				if let Ok(Some(start_time)) = plot_db.get_earliest_measurement(&aspect_id_clone).await {
					if let Ok(Some(end_time)) = plot_db.get_latest_measurement(&aspect_id_clone).await {
						// Use the user's selected view resolution for plot updates
						if let Ok(mut stream) = Outputs::analyze_range(&plot_db, &aspect_id_clone, start_time, end_time, view_resolution, Spline::Linear).await {
							let mut data = Vec::new();
							while let Some(result) = stream.next().await {
								if cancel_clone.load(Ordering::SeqCst) {
									break;
								}
								if let Ok(point) = result {
									data.push((point.timestamp, point.value.to_f64().unwrap_or(0.0)));
								}
							}
							if !data.is_empty() && !cancel_clone.load(Ordering::SeqCst) {
								debug!("Loaded {} measurements for plot update at {:?} resolution", data.len(), view_resolution);
								let _ = tx_clone.send(LoadingMessage::UpdateMeasurements(data)).await;
							}
						}
					}
				}
			}

			last_update_duration = update_start.elapsed();
			debug!("Plot update completed in {:?}", last_update_duration);
		}
	});

	for (chunk_idx, chunk) in measurements.chunks(chunk_size).enumerate() {
		if cancel.load(Ordering::SeqCst) {
			info!("CSV import cancelled at chunk {}", chunk_idx);
			return Ok(());
		}

		debug!("Importing chunk {} ({} measurements)", chunk_idx, chunk.len());

		// Import chunk (this triggers a WAL checkpoint)
		match db.batch_capture_measurements(aspect_id, dataset_id, chunk.to_vec()).await {
			Ok(_) => {
				debug!("Chunk {} imported successfully", chunk_idx);
			}
			Err(e) => {
				error!("Failed to import chunk {}: {}", chunk_idx, e);
				let _ = tx.send(LoadingMessage::Error(format!("Import failed at chunk {}: {}", chunk_idx, e))).await;
				return Err(e);
			}
		}

		// Send progress update (cheap, doesn't block import)
		let processed = (chunk_idx + 1) * chunk_size;
		if last_progress_update.elapsed() >= progress_interval {
			debug!("Progress: {}/{} measurements", processed, measurements.len());
			let percent = if !measurements.is_empty() { (processed as f64 / measurements.len() as f64 * 100.0) as u32 } else { 0 };
			let _ = tx.send(LoadingMessage::Progress { loaded: processed, scanned: measurements.len(), status: format!("Importing CSV: {}% complete ({} of {} rows)", percent, processed, measurements.len()) }).await;
			last_progress_update = Instant::now();
		}
	}

	// Stop the background plot update task
	cancel.store(true, Ordering::SeqCst);
	let _ = plot_task.await;

	info!("CSV import completed successfully: {} measurements", measurements.len());

	// Send completion message immediately - don't wait for final measurements to load
	// This prevents UI freeze on large datasets
	let _ = tx.send(LoadingMessage::Progress { loaded: measurements.len(), scanned: measurements.len(), status: "CSV import complete. Plot will update shortly...".to_string() }).await;
	let _ = tx.send(LoadingMessage::CompleteMeasurements(Vec::new())).await;

	// Load final measurements asynchronously in background
	// This prevents blocking the UI thread on large datasets
	let db_name_clone = db_name.clone();
	let aspect_id_clone = aspect_id;
	let tx_clone = tx.clone();
	let final_view_resolution = view_resolution;
	tokio::spawn(async move {
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;

		// Load final measurements in background without blocking import
		match Database::existing(&db_name_clone).await {
			Ok(final_db) => {
				if let Ok(Some(start)) = final_db.get_earliest_measurement(&aspect_id_clone).await {
					if let Ok(Some(end)) = final_db.get_latest_measurement(&aspect_id_clone).await {
						// Use the user's selected view resolution
						match Outputs::analyze_range(&final_db, &aspect_id_clone, start, end, final_view_resolution, Spline::Linear).await {
							Ok(mut stream) => {
								let mut data = Vec::new();
								while let Some(result) = stream.next().await {
									if let Ok(point) = result {
										data.push((point.timestamp, point.value.to_f64().unwrap_or(0.0)));
									}
								}
								if !data.is_empty() {
									debug!("Loaded {} measurements for final plot (background) at {:?} resolution", data.len(), final_view_resolution);
									let _ = tx_clone.send(LoadingMessage::UpdateMeasurements(data)).await;
								}
							}
							Err(e) => {
								debug!("Could not load final measurements (non-blocking): {}", e);
							}
						}
					}
				}
			}
			Err(e) => {
				debug!("Could not open database for final measurements (non-blocking): {}", e);
			}
		}
	});

	Ok(())
}

#[cfg(test)]
mod tests {
	use chrono::Datelike;

	use super::*;

	#[test]
	fn test_parse_unix_timestamp_seconds() {
		// Unix timestamp for 2012-01-01T00:01:00Z
		let result = parse_timestamp("1325412060");
		assert!(result.is_ok());
		let dt = result.unwrap();
		assert_eq!(dt.year(), 2012);
		assert_eq!(dt.month(), 1);
		assert_eq!(dt.day(), 1);
	}

	#[test]
	fn test_parse_unix_timestamp_milliseconds() {
		// Unix timestamp for 2012-01-01T00:01:00Z in milliseconds
		let result = parse_timestamp("1325412060000");
		assert!(result.is_ok());
		let dt = result.unwrap();
		assert_eq!(dt.year(), 2012);
		assert_eq!(dt.month(), 1);
		assert_eq!(dt.day(), 1);
	}

	#[test]
	fn test_parse_iso8601_format() {
		let result = parse_timestamp("2012-01-01T00:01:00Z");
		assert!(result.is_ok());
		let dt = result.unwrap();
		assert_eq!(dt.year(), 2012);
		assert_eq!(dt.month(), 1);
		assert_eq!(dt.day(), 1);
	}

	#[test]
	fn test_parse_common_format() {
		let result = parse_timestamp("2012-01-01 12:30:45");
		assert!(result.is_ok());
		let dt = result.unwrap();
		assert_eq!(dt.year(), 2012);
		assert_eq!(dt.month(), 1);
		assert_eq!(dt.day(), 1);
	}

	#[test]
	fn test_parse_invalid_timestamp() {
		let result = parse_timestamp("not a timestamp");
		assert!(result.is_err());
	}

	#[test]
	fn test_parse_floating_point_unix_timestamp() {
		// Test floating-point Unix timestamp like 1325412060.0
		let result = parse_timestamp("1325412060.0");
		assert!(result.is_ok());
		let dt = result.unwrap();
		assert_eq!(dt.year(), 2012);
		assert_eq!(dt.month(), 1);
		assert_eq!(dt.day(), 1);
	}

	#[test]
	fn test_parse_floating_point_with_decimals() {
		// Test floating-point Unix timestamp with fractional seconds
		let result = parse_timestamp("1325412060.123");
		assert!(result.is_ok());
		let dt = result.unwrap();
		assert_eq!(dt.year(), 2012);
		assert_eq!(dt.month(), 1);
		assert_eq!(dt.day(), 1);
	}
}
