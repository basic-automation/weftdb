//! The store root's `STORE_FORMAT` marker (freeze design §4.3, slice FRE-12a).
//!
//! A segment store root records which layout it is in, and which layouts a WeftDB must
//! know to read and to write it, in a small JSON file beside the control-plane
//! databases:
//!
//! ```json
//! {"kind":"weftdb-store","layout_version":2,"min_read_layout":2,"min_write_layout":2,
//!  "last_written_layout":2,"migrating_to":null,"applied_through":"0002_s6_s7",
//!  "store_uuid":"…","scope":{"database":"default","subject":"default"}}
//! ```
//!
//! The open reads it right after it takes the root's `LOCK` and before it opens any
//! database, and refuses a store whose `min_write_layout` is newer than
//! [`SUPPORTED_LAYOUT`] ([`StoreError::IncompatibleLayout`](crate::StoreError)). So a
//! WeftDB never runs its pragmas, its probes or its DDL on a store it would break, and a
//! Turso format change can be refused before Turso opens anything, by a migration that
//! raises both floors. A root that has databases but no marker (written before markers
//! existed, or its marker lost) is judged instead by the copy `segment_index.db`'s
//! `store_meta` keeps, read before anything is written to the root.
//!
//! - `layout_version` is the layout the store is in; `min_read_layout` and
//!   `min_write_layout` are the oldest layouts a WeftDB must know to read and to write
//!   it. 1.0 binaries have no read-only mode, so the read floor is recorded for future
//!   binaries and not acted on.
//! - `last_written_layout` is the layout of the last WeftDB that opened the store for
//!   writing; a newer WeftDB that finds it below `layout_version` re-derives what the
//!   migrations that declared older writers safe maintain.
//! - `migrating_to` is set while migrations run: the open raises the floors to what the
//!   pending migrations need *before* it runs them, so a migration that crashes half way
//!   leaves floors an older WeftDB already respects (floors are only ever too high,
//!   never too low).
//! - `applied_through` is the newest registered migration applied; `store_migrations`
//!   in `segment_index.db` is the authoritative applied set.
//! - `store_uuid` names the store; `scope` is the `(database, subject)` of the open that
//!   created the marker.
//!
//! Readers ignore fields they do not know, so a newer WeftDB can add some. The marker
//! is written through [`StoreFs`] (a temporary file, `sync_all`, a rename over the
//! marker, then an fsync of the root), so the power-cut tests see every write.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
	types::durable::{StoreFs, SyncPolicy, WritePoints}, StoreError
};

/// The marker's file name under the store root.
pub const STORE_FORMAT_FILE: &str = "STORE_FORMAT";

/// The temporary name a new marker is written under before it is renamed over
/// [`STORE_FORMAT_FILE`].
const STORE_FORMAT_TMP: &str = "STORE_FORMAT.tmp";

/// The `kind` of a segment store root's marker.
pub const STORE_KIND: &str = "weftdb-store";

/// The newest store layout this build reads and writes.
///
/// It goes up for a control-plane DDL layout, a new frame decode capability, or a Turso
/// on-disk format change (expressed as a migration that raises both floors). Layout
/// numbers are assigned at a release: every migration written before 1.0 belongs to
/// layout 2.
pub const SUPPORTED_LAYOUT: u32 = 2;

/// The layout of a store written before markers existed (any pre-1.0 WeftDB): no
/// `STORE_FORMAT`, control-plane databases present.
pub const LEGACY_LAYOUT: u32 = 1;

/// The `(database, subject)` scope a marker records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreScope {
	/// The database namespace.
	pub database: String,
	/// The subject namespace.
	pub subject: String,
}

/// The contents of a store root's `STORE_FORMAT` marker; see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreFormat {
	/// Always [`STORE_KIND`] for a segment store root.
	pub kind: String,
	/// The layout the store is in.
	pub layout_version: u32,
	/// The oldest layout a WeftDB must know to read the store.
	pub min_read_layout: u32,
	/// The oldest layout a WeftDB must know to write the store.
	pub min_write_layout: u32,
	/// The layout of the last WeftDB that opened the store for writing.
	pub last_written_layout: u32,
	/// The layout the open in progress is migrating to, while it migrates.
	#[serde(default)]
	pub migrating_to: Option<u32>,
	/// The newest registered migration applied, by id (`0002_s6_s7`).
	#[serde(default)]
	pub applied_through: Option<String>,
	/// The store's identity.
	pub store_uuid: String,
	/// The scope of the open that created the marker.
	pub scope: StoreScope,
}

impl StoreFormat {
	/// The marker a new store starts with: this build's layout and floors, migrating to
	/// it, with nothing applied yet.
	#[must_use]
	pub(crate) fn new_store(scope: StoreScope) -> Self {
		Self { kind: STORE_KIND.to_string(), layout_version: SUPPORTED_LAYOUT, min_read_layout: SUPPORTED_LAYOUT, min_write_layout: SUPPORTED_LAYOUT, last_written_layout: SUPPORTED_LAYOUT, migrating_to: Some(SUPPORTED_LAYOUT), applied_through: None, store_uuid: uuid::Uuid::new_v4().to_string(), scope }
	}

	/// What a store without a marker but with control-plane databases is: layout 1,
	/// written by a pre-1.0 WeftDB, with every floor at 1.
	#[must_use]
	pub(crate) fn legacy(scope: StoreScope) -> Self {
		Self { kind: STORE_KIND.to_string(), layout_version: LEGACY_LAYOUT, min_read_layout: LEGACY_LAYOUT, min_write_layout: LEGACY_LAYOUT, last_written_layout: LEGACY_LAYOUT, migrating_to: None, applied_through: None, store_uuid: uuid::Uuid::new_v4().to_string(), scope }
	}

	/// Whether this is the marker a new store starts with, still unsettled: an open that
	/// was creating the store stopped before it finished, and the next open finishes it.
	/// It is migrating to its own layout with nothing applied; a layout-1 store being
	/// migrated is migrating to a newer layout than its own.
	#[must_use]
	pub(crate) fn is_unfinished_creation(&self) -> bool {
		self.applied_through.is_none() && self.migrating_to == Some(self.layout_version)
	}

	/// Whether a WeftDB that writes layouts up to `supported` may write this store.
	#[must_use]
	pub const fn writable_by(&self, supported: u32) -> bool {
		self.min_write_layout <= supported
	}
}

/// The marker's path under `root`.
#[must_use]
pub fn marker_path(root: &Path) -> PathBuf {
	root.join(STORE_FORMAT_FILE)
}

/// Read `root`'s marker: `None` when there is none.
///
/// The marker is read with a plain file read, like every read of the store: a crash
/// cannot change what a read returns.
///
/// # Errors
///
/// [`StoreError::UnreadableStoreFormat`] when the file exists but is not a marker (not
/// JSON, a missing field, another `kind`), or cannot be read.
pub async fn read_marker(root: &Path) -> std::result::Result<Option<StoreFormat>, StoreError> {
	let path = marker_path(root);
	let unreadable = |reason: String| StoreError::UnreadableStoreFormat { path: path.clone(), reason };
	let bytes = match tokio::fs::read(&path).await {
		Ok(bytes) => bytes,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(unreadable(format!("reading it failed: {e}"))),
	};
	let format: StoreFormat = serde_json::from_slice(&bytes).map_err(|e| unreadable(format!("it is not a store marker: {e}")))?;
	if format.kind != STORE_KIND {
		return Err(unreadable(format!("it marks a {:?}, not a {STORE_KIND:?}", format.kind)));
	}
	Ok(Some(format))
}

/// Write `format` as `root`'s marker, durably.
///
/// The marker goes to a temporary file that is `sync_all`ed, renamed over the marker,
/// and the root fsynced. A crash at any point leaves either the previous marker or this
/// one (and maybe the temporary file, which the next write replaces).
///
/// # Errors
///
/// Any error encoding the marker or from `fs`.
pub async fn write_marker(fs: &dyn StoreFs, root: &Path, format: &StoreFormat) -> Result<()> {
	let tmp = root.join(STORE_FORMAT_TMP);
	let path = marker_path(root);
	let bytes = serde_json::to_vec(format).context("encoding the store marker")?;
	fs.remove_file(&tmp).await.with_context(|| format!("removing a stale {}", tmp.display()))?;
	fs.create_new_write(&tmp, bytes, SyncPolicy::Full, WritePoints::NONE).await.with_context(|| format!("writing {}", tmp.display()))?;
	fs.rename(&tmp, &path).await.with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
	fs.sync_dir(root).await.with_context(|| format!("fsyncing directory {}", root.display()))?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::durable::RealFs;

	fn scope() -> StoreScope {
		StoreScope { database: "default".into(), subject: "default".into() }
	}

	#[tokio::test]
	async fn a_marker_round_trips_and_a_missing_one_reads_as_none() {
		let dir = tempfile::tempdir().unwrap();
		assert_eq!(read_marker(dir.path()).await.unwrap(), None);
		let format = StoreFormat::new_store(scope());
		write_marker(&RealFs, dir.path(), &format).await.unwrap();
		assert_eq!(read_marker(dir.path()).await.unwrap(), Some(format.clone()));
		// A second write replaces it, through the same temporary name.
		let later = StoreFormat { migrating_to: None, applied_through: Some("0002_s6_s7".into()), ..format };
		write_marker(&RealFs, dir.path(), &later).await.unwrap();
		assert_eq!(read_marker(dir.path()).await.unwrap(), Some(later));
		assert!(!dir.path().join(STORE_FORMAT_TMP).exists(), "the temporary file was renamed away");
	}

	/// Readers ignore fields a newer WeftDB added, and a marker written as the design
	/// spells it parses.
	#[tokio::test]
	async fn unknown_fields_are_ignored() {
		let dir = tempfile::tempdir().unwrap();
		let text = r#"{"kind":"weftdb-store","layout_version":3,"min_read_layout":3,"min_write_layout":2,"last_written_layout":3,"migrating_to":null,"applied_through":"0009_future","store_uuid":"u","scope":{"database":"d","subject":"s","tenant":"t"},"series_catalog":true}"#;
		std::fs::write(dir.path().join(STORE_FORMAT_FILE), text).unwrap();
		let format = read_marker(dir.path()).await.unwrap().expect("a marker");
		assert_eq!((format.layout_version, format.min_read_layout, format.min_write_layout, format.applied_through.as_deref()), (3, 3, 2, Some("0009_future")));
		assert!(format.writable_by(SUPPORTED_LAYOUT) && !format.writable_by(1));
	}

	#[tokio::test]
	async fn a_marker_that_is_not_one_is_unreadable() {
		let dir = tempfile::tempdir().unwrap();
		for (text, why) in [("not json", "it is not a store marker"), (r#"{"kind":"weftdb-store","layout_version":2}"#, "missing field"), (r#"{"kind":"weftdb-legacy-database","layout_version":2,"min_read_layout":2,"min_write_layout":2,"last_written_layout":2,"store_uuid":"u","scope":{"database":"d","subject":"s"}}"#, "not a \"weftdb-store\"")] {
			std::fs::write(dir.path().join(STORE_FORMAT_FILE), text).unwrap();
			let err = read_marker(dir.path()).await.expect_err(text);
			let StoreError::UnreadableStoreFormat { path, reason } = &err else { panic!("{text}: {err:?}") };
			assert_eq!(path, &dir.path().join(STORE_FORMAT_FILE));
			assert!(reason.contains(why), "{text}: {reason}");
		}
	}
}
