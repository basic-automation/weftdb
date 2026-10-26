//! UCI corpus loader — the UCI Individual household electric power consumption
//! dataset, the real corpus the 1.0 plan's evidence scope names ("UCI
//! household-power data at 3 scales", `f085f84:docs/release/1.0-plan.md` §7
//! decision 18).
//!
//! File format (`household_power_consumption.txt`, semicolon-separated, one
//! minute-averaged row per minute over almost four years):
//!
//! ```text
//! Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3
//! 16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000
//! 28/4/2007;00:21:00;?;?;?;?;?;?;
//! ```
//!
//! - `Date` is `dd/mm/yyyy` (day and month are not zero-padded: `28/4/2007`
//!   occurs in the file) and `Time` is `hh:mm:ss`.
//! - `?` (or a missing trailing field) is a missing value. A row whose chosen
//!   column is missing is dropped from the loaded corpus and counted in
//!   [`UciCorpus::rows_missing`], so the ingest workload never has to invent a
//!   value for a gap — the file's gaps stay gaps, reported as
//!   [`UciCorpus::missingness_fraction`].
//! - The file carries no timezone, so timestamps are the naive wall-clock time
//!   read as UTC epoch seconds. That is a loading convention, not a claim about
//!   the household's timezone, and it is the same for every row, so ingest
//!   throughput and storage figures are unaffected by it.
//! - Values are kept as exact [`BigDecimal`] text, never rounded through `f64`,
//!   so the ingest workload can seal them under an exact scaled-integer schema
//!   (see [`crate::ingest`]) and gate correctness on an exact round-trip.

use std::{
	io::{BufRead, BufReader}, path::Path
};

use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveTime};

/// The nine columns of the UCI household-power file, in file order.
pub const UCI_COLUMNS: [&str; 9] = ["Date", "Time", "Global_active_power", "Global_reactive_power", "Voltage", "Global_intensity", "Sub_metering_1", "Sub_metering_2", "Sub_metering_3"];

/// The default projected column: household global minute-averaged active power
/// (kilowatt), the series the evidence scope's household-power figures use.
pub const DEFAULT_UCI_COLUMN: &str = "Global_active_power";

/// A loaded UCI column: time-sorted `(timestamp, value)` rows plus the load
/// accounting the report needs to stay honest about what was dropped.
#[derive(Debug, Clone)]
pub struct UciCorpus {
	/// The projected column name, exactly as it appears in the header.
	pub column: String,
	/// Epoch seconds (naive wall-clock read as UTC), strictly sorted ascending
	/// by the loader so a seal never sees a backwards batch.
	pub timestamps: Vec<i64>,
	/// Exact decimal values, parallel to [`Self::timestamps`].
	pub values: Vec<BigDecimal>,
	/// Data rows examined (including rows dropped for a missing column value).
	pub rows_seen: usize,
	/// Data rows dropped because the projected column was `?` or absent.
	pub rows_missing: usize,
	/// The smallest shared scaled-integer scale holding every loaded value
	/// exactly (max fractional digits after normalizing trailing zeros away).
	/// An ingest aspect declared as `ScaledI64 { scale: decimal_scale }` with
	/// tolerance `0` round-trips this corpus exactly.
	pub decimal_scale: u8,
}

impl UciCorpus {
	/// Share of examined rows dropped for a missing projected value.
	#[allow(clippy::cast_precision_loss)]
	pub fn missingness_fraction(&self) -> f64 {
		if self.rows_seen == 0 {
			0.0
		} else {
			self.rows_missing as f64 / self.rows_seen as f64
		}
	}
}

/// Load one column of a UCI household-power file, capping the data rows
/// examined at `max_rows` (missing rows inside the cap are dropped, so fewer
/// than `max_rows` rows may come back).
///
/// # Errors
///
/// Returns an error naming the file and line for a missing header, a header
/// that is not `Date;Time;...`, an unknown column, an unparseable date, time,
/// or value, or a value needing more than 18 fractional digits (beyond what a
/// scaled-integer ingest schema can hold exactly).
pub fn load_uci_corpus(path: &Path, column: &str, max_rows: usize) -> anyhow::Result<UciCorpus> {
	let file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
	load_uci_corpus_from_reader(BufReader::new(file), column, max_rows).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// The reader half of [`load_uci_corpus`], exposed so tests can load a fixture
/// from memory and so a caller that already holds the file need not re-open it.
pub fn load_uci_corpus_from_reader(reader: impl BufRead, column: &str, max_rows: usize) -> anyhow::Result<UciCorpus> {
	let mut lines = reader.lines();
	let header = lines.next().ok_or_else(|| anyhow::anyhow!("empty file: no header row"))?.map_err(|e| anyhow::anyhow!("cannot read header row: {e}"))?;
	let header_fields: Vec<&str> = header.split(';').map(str::trim).collect();
	anyhow::ensure!(header_fields.len() >= 2 && header_fields[0] == "Date" && header_fields[1] == "Time", "header row must start with `Date;Time`, got {header:?}");
	let column_index = header_fields.iter().position(|field| *field == column).ok_or_else(|| anyhow::anyhow!("no column {column:?} in header (columns: {})", header_fields.join(";")))?;

	let mut rows: Vec<(i64, BigDecimal)> = Vec::new();
	let mut rows_seen = 0_usize;
	let mut rows_missing = 0_usize;
	let mut decimal_scale = 0_u8;
	for (line_no, line) in lines.enumerate() {
		if rows_seen >= max_rows {
			break;
		}
		let line = line.map_err(|e| anyhow::anyhow!("line {}: {e}", line_no + 2))?;
		if line.trim().is_empty() {
			continue;
		}
		rows_seen += 1;
		let fields: Vec<&str> = line.split(';').collect();
		let date = fields.first().copied().unwrap_or("").trim();
		let time = fields.get(1).copied().unwrap_or("").trim();
		let timestamp = parse_uci_timestamp(date, time).map_err(|e| anyhow::anyhow!("line {}: {e}", line_no + 2))?;
		let raw = fields.get(column_index).copied().unwrap_or("").trim();
		if raw.is_empty() || raw == "?" {
			rows_missing += 1;
			continue;
		}
		let value: BigDecimal = raw.parse().map_err(|e| anyhow::anyhow!("line {}: value {raw:?}: {e}", line_no + 2))?;
		let scale = value.normalized().as_bigint_and_exponent().1;
		anyhow::ensure!(scale >= 0 && scale <= 18, "line {}: value {raw:?} needs {scale} fractional digits, over the 18-digit exact-scale cap", line_no + 2);
		decimal_scale = decimal_scale.max(u8::try_from(scale).unwrap_or(u8::MAX));
		rows.push((timestamp, value));
	}
	rows.sort_by_key(|(timestamp, _)| *timestamp);
	let (timestamps, values): (Vec<i64>, Vec<BigDecimal>) = rows.into_iter().unzip();
	Ok(UciCorpus { column: column.to_string(), timestamps, values, rows_seen, rows_missing, decimal_scale })
}

/// Parse a UCI `dd/mm/yyyy` date and `hh:mm:ss` time into epoch seconds (naive
/// wall-clock read as UTC). Day and month need not be zero-padded.
fn parse_uci_timestamp(date: &str, time: &str) -> anyhow::Result<i64> {
	let date_parts: Vec<&str> = date.split('/').collect();
	let time_parts: Vec<&str> = time.split(':').collect();
	anyhow::ensure!(date_parts.len() == 3, "unparseable date {date:?} (expected dd/mm/yyyy)");
	anyhow::ensure!(time_parts.len() == 3, "unparseable time {time:?} (expected hh:mm:ss)");
	let parse = |part: &str, what: &str| part.parse::<u32>().map_err(|_| anyhow::anyhow!("unparseable {what} {part:?}"));
	let (day, month, year) = (parse(date_parts[0], "day")?, parse(date_parts[1], "month")?, parse(date_parts[2], "year")?);
	let (hour, minute, second) = (parse(time_parts[0], "hour")?, parse(time_parts[1], "minute")?, parse(time_parts[2], "second")?);
	let date = NaiveDate::from_ymd_opt(i32::try_from(year).map_err(|_| anyhow::anyhow!("year {year} out of range"))?, month, day).ok_or_else(|| anyhow::anyhow!("invalid calendar date {date:?}"))?;
	let time = NaiveTime::from_hms_opt(hour, minute, second).ok_or_else(|| anyhow::anyhow!("invalid clock time {time:?}"))?;
	Ok(date.and_time(time).and_utc().timestamp())
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use super::*;

	/// The file's own first rows (plus a gap row in its own shape), as a fixture.
	const FIXTURE: &str = "Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3\n\
		16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000\n\
		16/12/2006;17:25:00;5.360;0.436;233.630;23.000;0.000;1.000;16.000\n\
		28/4/2007;00:20:00;0.492;0.208;236.240;2.200;0.000;0.000;0.000\n\
		28/4/2007;00:21:00;?;?;?;?;?;?;\n";

	#[test]
	fn loads_the_corpus_it_claims_to_load() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
		assert_eq!(corpus.column, "Global_active_power");
		// 16/12/2006 17:24:00 UTC and 28/4/2007 00:20:00 UTC, as epoch seconds.
		assert_eq!(corpus.timestamps, vec![1_166_289_840, 1_166_289_900, 1_177_719_600]);
		assert_eq!(corpus.values.iter().map(BigDecimal::to_plain_string).collect::<Vec<_>>(), vec!["4.216", "5.360", "0.492"]);
		assert_eq!(corpus.rows_seen, 4);
		assert_eq!(corpus.rows_missing, 1);
		assert_eq!(corpus.decimal_scale, 3);
		assert!((corpus.missingness_fraction() - 0.25).abs() < f64::EPSILON);
	}

	#[test]
	fn other_columns_project_by_header_name_not_position() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), "Voltage", usize::MAX).expect("loads");
		assert_eq!(corpus.values.iter().map(BigDecimal::to_plain_string).collect::<Vec<_>>(), vec!["234.840", "233.630", "236.240"]);
		assert_eq!(corpus.rows_missing, 1, "the gap row is missing in every column");
	}

	#[test]
	fn rejects_an_unknown_column_a_bad_date_and_a_bad_value() {
		assert!(load_uci_corpus_from_reader(Cursor::new(FIXTURE), "Nope", usize::MAX).is_err());
		let bad_date = FIXTURE.replace("16/12/2006", "32/12/2006");
		assert!(load_uci_corpus_from_reader(Cursor::new(bad_date), DEFAULT_UCI_COLUMN, usize::MAX).is_err());
		let bad_value = FIXTURE.replace("4.216", "four");
		assert!(load_uci_corpus_from_reader(Cursor::new(bad_value), DEFAULT_UCI_COLUMN, usize::MAX).is_err());
		assert!(load_uci_corpus_from_reader(Cursor::new(""), DEFAULT_UCI_COLUMN, usize::MAX).is_err());
		assert!(load_uci_corpus_from_reader(Cursor::new("Foo;Bar\n"), DEFAULT_UCI_COLUMN, usize::MAX).is_err());
	}

	#[test]
	fn max_rows_caps_examined_rows_not_loaded_rows() {
		let corpus = load_uci_corpus_from_reader(Cursor::new(FIXTURE), DEFAULT_UCI_COLUMN, 2).expect("loads");
		assert_eq!(corpus.rows_seen, 2);
		assert_eq!(corpus.timestamps.len(), 2);
	}
}
