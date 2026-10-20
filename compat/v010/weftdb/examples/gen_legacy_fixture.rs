//! Write a legacy rows-mode database exactly as the v0.1.0 library does.
//!
//! This is the generator behind `compat/fixtures/v0.1.0/legacy/` (1.0 plan, slice X-FX). It
//! is compiled from the `v0.1.0` tag, never from a later tree, so what it writes is what the
//! first public release wrote; `compat/v010/README.md` records how to build and run it. It is
//! deliberately small: the point is the on-disk shape of a v0.1.0 rows-mode database (its
//! control-plane files and its rows), not volume.
//!
//! ```text
//! WEFT_DATA_DIR=/tmp/weft-v010-fixture/legacy \
//!   cargo run --locked --release --example gen_legacy_fixture -- v010_legacy rows.csv
//! ```
//!
//! The database lands at `$WEFT_DATA_DIR/v010_legacy` (`WEFT_DATA_DIR` is how a v0.1.0 caller
//! chose where databases lived). `rows.csv` is the shared grid the fixture script writes:
//! `subject,aspect,timestamp,value` with no header. The same file drives the Storage-v2
//! fixture's HTTP ingest, so the two trees hold the same values — except that rows-mode has
//! no null, so a null row in the grid is skipped and reported here, and the legacy tree
//! carries the non-null subset.
//!
//! The tree it leaves behind is v0.1.0's own rows-mode layout — `<data_dir>/<db>/metadata.db`
//! (turso) plus one directory per subject and aspect, each aspect holding `measurements.db`
//! and its neighbours — because it writes through the release's write path rather than
//! assembling files by hand.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeZone, Utc};
use weftdb::database::traits::{AspectStructure, DatabaseStructure, Inputs};
use weftdb::{AspectId, DatasetId, Database, InputMeasurement, Resolution, SubjectId};

/// One row of `rows.csv`.
struct Row {
	subject: String,
	aspect: String,
	when: DateTime<Utc>,
	value: BigDecimal,
}

/// The grid's rows, plus how many were dropped for a shape this storage mode cannot hold.
struct RowReader {
	rows: Vec<Row>,
	skipped_nulls: usize,
}

fn read_rows(path: &Path) -> Result<RowReader> {
	let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
	let mut rows = Vec::new();
	let mut skipped = 0usize;
	for (number, line) in text.lines().filter(|line| !line.trim().is_empty()).enumerate() {
		let fields: Vec<&str> = line.split(',').map(str::trim).collect();
		if fields.len() != 4 {
			bail!("{} line {}: expected subject,aspect,timestamp,value in {line:?}", path.display(), number + 1);
		}
		let seconds: i64 = fields[2].parse().with_context(|| format!("line {}: timestamp {}", number + 1, fields[2]))?;
		let when = Utc.timestamp_opt(seconds, 0).single().with_context(|| format!("line {}: timestamp out of range", number + 1))?;
		// Rows-mode has no null: a v0.1.0 `InputMeasurement` always carries a decimal. The
		// shared grid holds null rows (an empty value) because Storage-v2 seals them, so a
		// null line is skipped and counted here rather than failing the whole grid.
		if fields[3].is_empty() {
			skipped += 1;
			continue;
		}
		rows.push(Row { subject: fields[0].to_string(), aspect: fields[1].to_string(), when, value: BigDecimal::from_str(fields[3]).with_context(|| format!("line {}: value {}", number + 1, fields[3]))? });
	}
	Ok(RowReader { rows, skipped_nulls: skipped })
}

#[tokio::main]
async fn main() -> Result<()> {
	let mut args = std::env::args().skip(1);
	let name = args.next().context("usage: gen_legacy_fixture <database-name> <rows.csv>")?;
	let csv = args.next().context("usage: gen_legacy_fixture <database-name> <rows.csv>")?;
	let RowReader { rows, skipped_nulls } = read_rows(Path::new(&csv))?;
	if rows.is_empty() {
		bail!("{csv} holds no row");
	}

	let db = Database::new(&name).await?;
	// The file's order defines the names the catalog gets, so the fixture's catalog is
	// reproducible from rows.csv alone.
	let mut subjects: HashMap<String, SubjectId> = HashMap::new();
	let mut aspects: HashMap<(String, String), AspectId> = HashMap::new();
	let mut written = 0usize;
	for row in &rows {
		let subject = match subjects.get(&row.subject) {
			Some(id) => *id,
			None => {
				let id = db.observe_subject(&row.subject).await?.id();
				subjects.insert(row.subject.clone(), id);
				id
			}
		};
		let aspect = match aspects.get(&(row.subject.clone(), row.aspect.clone())) {
			Some(id) => *id,
			None => {
				let id = db.track_aspect(&subject, &row.aspect, &Resolution::Seconds, None).await?.id();
				aspects.insert((row.subject.clone(), row.aspect.clone()), id);
				id
			}
		};
		db.capture_measurement(&aspect, &DatasetId::new(), &InputMeasurement::new(row.when, row.value.clone())).await?;
		written += 1;
	}

	let mut per_aspect: HashMap<String, usize> = HashMap::new();
	for row in &rows {
		*per_aspect.entry(row.aspect.clone()).or_insert(0) += 1;
	}
	let mut summary: Vec<String> = per_aspect.iter().map(|(aspect, count)| format!("{aspect}={count}")).collect();
	summary.sort();
	println!("wrote {name}: {written} row in {}; {skipped_nulls} null row skipped, rows-mode has no null", summary.join(" "));
	Ok(())
}
