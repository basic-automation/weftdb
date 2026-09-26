use serde::{Deserialize, Serialize};

/// Type of event detector.
///
/// Used to determine how a detector can be reconstructed on load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectorType {
	/// Built-in detector with type name (e.g., "`monthly_increase`", "peak", "valley").
	/// Can be auto-reconstructed from config on load.
	Builtin(String),
	/// Custom detector requiring re-registration at runtime.
	/// The detector function must be provided by the user after load.
	Custom,
	/// Script-based detector (reserved for future use).
	/// Scripts can be stored and executed at runtime.
	Script {
		/// The scripting engine (e.g., "rhai", "lua")
		engine: String,
		/// The script source code
		source: String,
	},
}

impl DetectorType {
	/// Returns true if this detector can be auto-reconstructed on load.
	#[must_use]
	pub const fn is_reconstructible(&self) -> bool {
		matches!(self, Self::Builtin(_) | Self::Script { .. })
	}

	/// Returns true if this detector requires manual re-registration.
	#[must_use]
	pub const fn requires_registration(&self) -> bool {
		matches!(self, Self::Custom)
	}
}

/// Metadata for a registered event detector (persisted to DB).
///
/// The actual detector function cannot be serialized, so only metadata
/// is stored. Detectors must be re-registered at runtime for `Custom` types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectorMetadata {
	/// Unique detector identifier
	id: String,
	/// Human-readable name
	name: String,
	/// Optional description
	description: Option<String>,
	/// Detector type for reconstruction
	detector_type: DetectorType,
	/// JSON configuration for builtin detectors (e.g., threshold values)
	config_json: Option<String>,
}

impl DetectorMetadata {
	/// Creates new detector metadata.
	#[must_use]
	pub fn new(id: impl Into<String>, name: impl Into<String>, description: Option<String>, detector_type: DetectorType, config_json: Option<String>) -> Self {
		Self { id: id.into(), name: name.into(), description, detector_type, config_json }
	}

	/// Creates metadata for a custom detector.
	#[must_use]
	pub fn custom(id: impl Into<String>, name: impl Into<String>, description: Option<String>) -> Self {
		Self { id: id.into(), name: name.into(), description, detector_type: DetectorType::Custom, config_json: None }
	}

	/// Creates metadata for a builtin detector.
	#[must_use]
	pub fn builtin(id: impl Into<String>, name: impl Into<String>, description: Option<String>, builtin_type: impl Into<String>, config_json: Option<String>) -> Self {
		Self { id: id.into(), name: name.into(), description, detector_type: DetectorType::Builtin(builtin_type.into()), config_json }
	}

	/// Returns the detector ID.
	#[must_use]
	pub fn detector_id(&self) -> &str {
		&self.id
	}

	/// Returns the detector name.
	#[must_use]
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Returns the detector description.
	#[must_use]
	pub fn description(&self) -> Option<&str> {
		self.description.as_deref()
	}

	/// Returns the detector type.
	#[must_use]
	pub const fn detector_type(&self) -> &DetectorType {
		&self.detector_type
	}

	/// Returns the config JSON.
	#[must_use]
	pub fn config_json(&self) -> Option<&str> {
		self.config_json.as_deref()
	}
}
