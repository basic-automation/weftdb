//! The failure tests (#147) end to end: the harness kills a real child process (this crate's
//! binary) and checks the store it leaves behind.

use std::io::Write as _;

use weft_bench::{
	failure::{run_failure, Failure, FailureParams}, load_uci_corpus, IngestProfile, DEFAULT_UCI_COLUMN
};

const FIXTURE: &str = "Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3\n\
	16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000\n\
	16/12/2006;17:25:00;5.360;0.436;233.630;23.000;0.000;1.000;16.000\n\
	16/12/2006;17:26:00;5.374;0.498;233.290;23.000;0.000;2.000;17.000\n\
	16/12/2006;17:27:00;5.388;0.502;233.740;23.000;0.000;1.000;17.000\n\
	16/12/2006;17:28:00;3.666;0.528;235.680;15.800;0.000;1.000;17.000\n";

#[tokio::test]
async fn every_failure_leaves_a_store_that_holds_what_it_acknowledged() {
	let dir = std::env::temp_dir().join(format!("weft-bench-failure-it-{}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("dir");
	let uci = dir.join("uci.txt");
	std::fs::File::create(&uci).and_then(|mut f| f.write_all(FIXTURE.as_bytes())).expect("fixture");
	let corpus = load_uci_corpus(&uci, DEFAULT_UCI_COLUMN, usize::MAX).expect("loads");
	let profile = IngestProfile::new("it", "global_active_power", 16);
	let params = FailureParams { uci: uci.clone(), column: DEFAULT_UCI_COLUMN.to_string(), corpus_rows: usize::MAX, rows: 400, reps: 2, compact_to: 200, kill_within: std::time::Duration::from_millis(20), seed: 5, exe: env!("CARGO_BIN_EXE_weft-bench").into() };
	for failure in Failure::ALL {
		let result = run_failure(failure, &profile, &corpus.timestamps, &corpus.values, corpus.decimal_scale, &params).await.unwrap_or_else(|e| panic!("{}: {e:#}", failure.as_str()));
		assert!(result.correctness.passed(), "{}: {:?}", failure.as_str(), result.correctness);
		assert_eq!((result.workload.as_str(), result.reps), ("failure", 2));
		assert!(result.correctness.result_sha256.is_some());
	}
	let _ = std::fs::remove_dir_all(&dir);
}
