//! The crash matrix (D-S21, SOA-9 `crash_kill9`) end to end: every failure
//! test, at more than one seed, judged as one matrix. This is the test the
//! crash-matrix CI job leans on for its gate — a small matrix here, so the
//! full seed set stays in `scripts/crash-matrix.sh`, where a slow run does
//! not hold up every pull request.
#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use std::io::Write as _;

use weft_bench::{
	crash_matrix::{all_passed, run_matrix}, failure::{Failure, FailureParams}, load_uci_corpus, IngestProfile, DEFAULT_UCI_COLUMN
};

const FIXTURE: &str = "Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3\n\
	16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000\n\
	16/12/2006;17:25:00;5.360;0.436;233.630;23.000;0.000;1.000;16.000\n\
	16/12/2006;17:26:00;5.374;0.498;233.290;23.000;0.000;2.000;17.000\n\
	16/12/2006;17:27:00;5.388;0.502;233.740;23.000;0.000;1.000;17.000\n\
	16/12/2006;17:28:00;3.666;0.528;235.680;15.800;0.000;1.000;17.000\n";

#[tokio::test]
async fn every_failure_at_every_matrix_seed_holds_what_it_acknowledged() {
	let dir = std::env::temp_dir().join(format!("weft-bench-crash-matrix-it-{}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("dir");
	let uci = dir.join("uci.txt");
	std::fs::File::create(&uci).and_then(|mut f| f.write_all(FIXTURE.as_bytes())).expect("fixture");
	let corpus = load_uci_corpus(&uci, DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
	let profile = IngestProfile::new("it", "global_active_power", 16);
	let params = FailureParams { uci: uci.clone(), column: DEFAULT_UCI_COLUMN.to_string(), corpus_rows: usize::MAX, rows: 400, reps: 1, compact_to: 200, kill_within: std::time::Duration::from_millis(20), seed: 5, exe: env!("CARGO_BIN_EXE_weft-bench").into() };
	let cells = run_matrix(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, &params, &[5, 6]).await.expect("matrix runs");
	assert_eq!(cells.len(), Failure::ALL.len() * 2, "one cell per failure per seed");
	for cell in &cells {
		assert!(cell.passed(), "{} seed {}: {:?}", cell.failure.as_str(), cell.seed, cell.result.correctness);
		assert_eq!(cell.result.dataset.seed, cell.seed, "the cell ran at its own seed");
	}
	assert!(all_passed(&cells));
	let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_seed_list_is_not_a_vacuous_pass() {
	let dir = std::env::temp_dir().join(format!("weft-bench-crash-matrix-empty-{}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("dir");
	let uci = dir.join("uci.txt");
	std::fs::File::create(&uci).and_then(|mut f| f.write_all(FIXTURE.as_bytes())).expect("fixture");
	let corpus = load_uci_corpus(&uci, DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
	let profile = IngestProfile::new("it", "global_active_power", 16);
	let params = FailureParams { uci: uci.clone(), column: DEFAULT_UCI_COLUMN.to_string(), corpus_rows: usize::MAX, rows: 400, reps: 1, compact_to: 200, kill_within: std::time::Duration::from_millis(20), seed: 5, exe: env!("CARGO_BIN_EXE_weft-bench").into() };
	assert!(run_matrix(&profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, &params, &[]).await.is_err());
	assert!(!all_passed(&[]));
	let _ = std::fs::remove_dir_all(&dir);
}
