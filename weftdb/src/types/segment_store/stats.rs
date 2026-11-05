//! Segment-store implementation: this submodule holds the `stats` portion of [`SegmentStore`](super::SegmentStore).
//! Pure move of the former `segment_store.rs` `impl` block; no behaviour change.

use super::*;

impl SegmentStore {
	/// How many segment-index transaction attempts have lost an MVCC conflict (or found
	/// the database busy) since this store opened, whether the transaction was then
	/// retried or failed.
	///
	/// The store's own writers do not conflict with each other: the writers of one aspect
	/// take turns under its commit lock, and those of different aspects touch different
	/// rows. So this stays at zero unless something else contends for
	/// `segment_index.db`, and a count that rises is a conflict nothing explains.
	#[must_use]
	pub fn index_conflicts(&self) -> u64 {
		self.index_conflicts.load(Ordering::Relaxed)
	}

	/// Number of sealed segments recorded for `aspect`.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn segment_count(&self, aspect: &str) -> Result<usize> {
		self.index.count(aspect).await
	}

	/// Realized storage accounting for `aspect` — the north-star **bytes/point** term
	/// (priority #1 in the commercial thesis) measured over the segments actually on
	/// disk, plus the segment/row counts and the covered time span.
	///
	/// Computed from the resident [`SegmentIndex`](weft_physical_type::SegmentIndex)
	/// (the descriptors' recorded framed byte lengths and row counts), so it reflects
	/// the realized `.weftseg` files including their header/index/checksum overhead,
	/// not an advisory column estimate. No segment file is opened.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn aspect_stats(&self, aspect: &str) -> Result<AspectStorageStats> {
		// One read of the rows: the whole index for the aspect-wide figures, and the
		// per-series overlap count (TAG-6), which for an untagged aspect is the same.
		let rows = self.index.rows(aspect).await?;
		let overlapping_segments = series_overlapping_count(&rows);
		let mut index = weft_physical_type::SegmentIndex::new();
		for row in rows {
			index.push(row.desc);
		}
		Ok(AspectStorageStats { segment_count: index.len(), total_rows: index.total_rows(), total_bytes: index.total_bytes(), bytes_per_point: index.bytes_per_point(), time_range: index.time_range(), unsorted_segments: index.unsorted_count(), overlapping_segments })
	}
}
