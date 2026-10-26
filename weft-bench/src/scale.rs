//! Named size presets for the generated workloads.
//!
//! "Run it at scale" should not mean remembering four numbers. A preset names a size and
//! expands it into the knobs of whichever generated workload is running, so two runs of the
//! same preset are comparable across machines and across releases, and a report says which
//! preset produced it rather than leaving a reader to infer size from four flags.
//!
//! A preset never overrides a knob the caller passed explicitly: it fills a size knob that is
//! still at its default and leaves any other value alone, so `--scale large --sp-rows 64`
//! rescales the grid around the block size you named rather than discarding it. (A knob set to
//! exactly its default value is, by definition, indistinguishable from one left alone.)
//!
//! The budgets are values, not bytes: 8 bytes for the `f64` plus a bit of validity per value,
//! so `Large`'s 16 777 216 values are about 134 MiB of column data before the store's own
//! overhead.

use crate::sparse::SparseParams;

/// The preset used when a workload wants a size but none was named.
pub const DEFAULT_SCALE_PRESET: ScalePreset = ScalePreset::Small;

/// A named size for the generated workloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScalePreset {
	/// A smoke-test table: fits in cache, runs in milliseconds.
	Tiny,
	/// The default workload size: a day of one-second telemetry per series.
	Small,
	/// A week of telemetry per series: seconds, not milliseconds.
	Medium,
	/// A month per series across many series: the size a laptop stops being interesting at.
	Large,
}

/// How many presets there are.
const PRESET_COUNT: usize = 4;

impl ScalePreset {
	/// Every preset, smallest first.
	#[must_use]
	pub const fn all() -> [Self; PRESET_COUNT] {
		[Self::Tiny, Self::Small, Self::Medium, Self::Large]
	}

	/// Parse a preset name, case- and whitespace-insensitive.
	#[must_use]
	pub fn parse(name: &str) -> Option<Self> {
		let normalized: String = name.trim().chars().filter(|c| *c != '-' && *c != '_').collect::<String>().to_lowercase();
		Self::all().into_iter().find(|preset| preset.as_str() == normalized)
	}

	/// The preset's canonical name, as `--scale` accepts and reports print.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Tiny => "tiny",
			Self::Small => "small",
			Self::Medium => "medium",
			Self::Large => "large",
		}
	}

	/// The list of accepted names, for error messages and help text.
	#[must_use]
	pub fn names() -> String {
		Self::all().iter().map(|preset| preset.as_str()).collect::<Vec<_>>().join(", ")
	}

	/// Total values the block-missingness table holds at this preset, across all series.
	#[must_use]
	pub const fn value_budget(self) -> usize {
		match self {
			Self::Tiny => 65_536,
			Self::Small => 524_288,
			Self::Medium => 4_194_304,
			Self::Large => 16_777_216,
		}
	}

	/// The sample count the series-generating workloads (gap-fill, downsample) use at this
	/// preset: one second apart, so an hour, a day, a week and a month respectively.
	#[must_use]
	pub const fn point_count(self) -> usize {
		match self {
			Self::Tiny => 3_600,
			Self::Small => 86_400,
			Self::Medium => 604_800,
			Self::Large => 2_592_000,
		}
	}

	/// Rescale a block-missingness grid to this preset.
	///
	/// `rows_per_block` is kept as given — it is the block size under test, not a size knob —
	/// and the series count grows as the square root of the block count (capped at 64), because
	/// a real wide table gains history before it gains devices. The resulting total is the
	/// preset's [`Self::value_budget`] rounded down to whole blocks.
	#[must_use]
	pub fn sparse(self, base: &SparseParams) -> SparseParams {
		let rows = base.rows_per_block.max(1);
		let total_blocks = (self.value_budget() / rows).max(1);
		let series = total_blocks.isqrt().clamp(1, 64);
		SparseParams { series, blocks: (total_blocks / series).max(1), ..base.clone() }
	}

	/// One-line description of what the preset expands to, for `--help` and run output.
	#[must_use]
	pub fn describe(self) -> String {
		format!("{} values across a sparse grid; {} samples for a generated series", self.value_budget(), self.point_count())
	}
}

impl std::fmt::Display for ScalePreset {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_str())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sparse::SparseParams;

	#[test]
	fn parsing_round_trips_and_refuses_junk() {
		for preset in ScalePreset::all() {
			assert_eq!(ScalePreset::parse(preset.as_str()), Some(preset));
			assert_eq!(ScalePreset::parse(&preset.as_str().to_uppercase()), Some(preset), "names are case-insensitive");
			assert_eq!(ScalePreset::parse(&format!("  {}  ", preset.as_str())), Some(preset), "surrounding space is fine");
		}
		assert_eq!(ScalePreset::parse("huge"), None);
		assert_eq!(ScalePreset::parse(""), None);
	}

	#[test]
	fn sizes_increase_in_every_dimension() {
		let presets = ScalePreset::all();
		for window in presets.windows(2) {
			assert!(window[0].value_budget() < window[1].value_budget(), "{} should hold fewer values than {}", window[0], window[1]);
			assert!(window[0].point_count() < window[1].point_count(), "{} should generate fewer samples than {}", window[0], window[1]);
		}
		assert_eq!(ScalePreset::parse("medium"), Some(ScalePreset::Medium));
	}

	#[test]
	fn sparse_keeps_the_block_size_and_fills_the_budget() {
		for preset in ScalePreset::all() {
			let params = preset.sparse(&SparseParams::default());
			assert_eq!(params.rows_per_block, SparseParams::default().rows_per_block, "the block size under test is not a size knob");
			assert!(params.series >= 1 && params.blocks >= 1);
			let rows = params.rows_per_block;
			let total = params.series * params.blocks * rows;
			assert!(total <= preset.value_budget(), "{} overshoots its budget: {total}", preset);
			// At most one block's worth of slack per series: the budget is spent, not rounded
			// away.
			assert!(preset.value_budget() - total < params.series * rows, "{} wastes a large share of its budget: {total} of {}", preset, preset.value_budget());
		}
	}

	#[test]
	fn sparse_scales_the_grid_not_one_dimension_alone() {
		let base = SparseParams { rows_per_block: 64, ..SparseParams::default() };
		let tiny = ScalePreset::Tiny.sparse(&base);
		let medium = ScalePreset::Medium.sparse(&base);
		assert!(medium.series > tiny.series, "a bigger preset widens the table while there is width to add: {} vs {}", tiny.series, medium.series);
		assert!(medium.blocks > tiny.blocks, "and deepens it");
		let large = ScalePreset::Large.sparse(&base);
		assert!(large.series <= 64, "the shape stays a wide table rather than a transpose: {}", large.series);
		assert!(large.blocks > medium.blocks, "past the 64-series cap the extra budget goes into blocks");
		assert_eq!(large.rows_per_block, 64, "a non-default block size survives");
	}

	#[test]
	fn describe_names_both_numbers() {
		let line = ScalePreset::Medium.describe();
		assert!(line.contains("4194304"), "{line}");
		assert!(line.contains("604800"), "{line}");
		assert!(ScalePreset::names().contains("tiny, small, medium, large"));
	}
}
