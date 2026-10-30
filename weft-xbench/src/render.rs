//! Report renderer with a generated honesty section (evidence track EVI-15R).
//!
//! Turns an [`XbenchReport`] into Markdown and a self-contained HTML page. The honesty section is
//! generated from the results rather than written beside them, so it cannot drift from the
//! numbers. It has the six parts the Weft-Bench report requirements name: where WeftDB wins, where
//! it loses, workloads not tested, known limitations, configuration caveats and reproduction.
//!
//! Wins and losses are decided by the p50 bootstrap intervals, not the point estimates. A WeftDB arm
//! wins only when its interval lies wholly below the fastest other engine's, and loses only when it
//! lies wholly above. Overlapping intervals are reported as within noise. An arm that failed its
//! correctness gate is never compared: its timing is listed under the failures, first.

use std::fmt::Write as _;

use weft_bench::stats::ConfidenceInterval;

use crate::{ArmResult, DatasetKind, Phase, XbenchReport};

/// Datasets the first report's scope (decision 18) asks for.
const EXPECTED_KINDS: [DatasetKind; 3] = [DatasetKind::Mcar, DatasetKind::Block, DatasetKind::Uci];
/// How many scales decision 18 asks for.
const EXPECTED_SCALES: usize = 3;
/// Arms decision 18 asks for: WeftDB stored, in-memory and `f64`; `DuckDB` `DOUBLE` and `DECIMAL`;
/// and, from the second external engine on, `ClickHouse` `Float64` and `Decimal64`.
const EXPECTED_ARMS: [&str; 8] = ["weftdb-memory", "weftdb-stored", "weftdb-stored-f64", "weftdb-stored-bulk", "duckdb-double", "duckdb-decimal", "clickhouse-double", "clickhouse-decimal"];
/// Below this many reps the bootstrap intervals are too coarse to separate much.
const FEW_REPS: usize = 10;

/// Workloads and engines outside this benchmark, listed in every report so a reader does not take
/// silence for coverage.
const NOT_IN_SCOPE: [&str; 6] = ["Gap fill, point and range reads: measured by `weft-bench` against WeftDB's own baselines, not against another engine.", "Spline (quadratic, cubic) reconstruction: neither DuckDB nor ClickHouse has a spline, so only linear interpolation is compared.", "Concurrent ingest and query, many series, and long-running behaviour: covered by the soak harness, not by this report.", "QuestDB, InfluxDB 3 Core, TimescaleDB and IoTDB: no adapter yet. Enterprise editions of QuestDB and InfluxDB 3 are excluded by their licences (see WILLINGNESS-TO-PAY.md).", "kdb+ and AVEVA PI: their licences bar benchmarking or publishing results without written consent.", "GPU backends: every arm runs splimes' automatic backend selection; GPU-specific tuning is a separate report."];

/// How one WeftDB arm compares with the fastest other engine on one dataset and phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
	/// WeftDB's p50 interval lies wholly below the other engine's.
	Win,
	/// WeftDB's p50 interval lies wholly above the other engine's.
	Loss,
	/// The intervals overlap.
	Noise,
}

/// One WeftDB arm against the fastest passing non-WeftDB arm.
#[derive(Debug, Clone)]
pub struct Verdict<'a> {
	/// The WeftDB result.
	pub weft: &'a ArmResult,
	/// The other engine's fastest passing result on the same dataset and phase.
	pub other: &'a ArmResult,
	/// Win, loss or noise.
	pub outcome: Outcome,
}

impl Verdict<'_> {
	/// How many times faster the faster of the two is, by p50.
	#[must_use]
	pub fn ratio(&self) -> f64 {
		let (a, b) = (ns(self.weft.latency_ci.p50.point_ns), ns(self.other.latency_ci.p50.point_ns));
		if a <= 0.0 || b <= 0.0 {
			return 1.0;
		}
		a.max(b) / a.min(b)
	}
}

#[allow(clippy::cast_precision_loss)]
const fn ns(v: u64) -> f64 {
	v as f64
}

fn is_weft(r: &ArmResult) -> bool {
	r.arm.arm.starts_with("weftdb")
}

fn passed(r: &ArmResult) -> bool {
	r.correctness.as_ref().is_none_or(|c| c.passed)
}

/// Compare two p50 intervals.
#[must_use]
pub const fn compare(weft: &ConfidenceInterval, other: &ConfidenceInterval) -> Outcome {
	if weft.upper_ns < other.lower_ns {
		Outcome::Win
	} else if other.upper_ns < weft.lower_ns {
		Outcome::Loss
	} else {
		Outcome::Noise
	}
}

/// Every passing WeftDB result against the fastest passing other-engine result on the same dataset
/// and phase. Datasets and phases with no other engine produce no verdict.
#[must_use]
pub fn verdicts(report: &XbenchReport) -> Vec<Verdict<'_>> {
	let mut out = Vec::new();
	for weft in report.results.iter().filter(|r| is_weft(r) && passed(r)) {
		let other = report.results.iter().filter(|r| !is_weft(r) && passed(r) && r.dataset == weft.dataset && r.phase == weft.phase).min_by_key(|r| r.latency_ci.p50.point_ns);
		if let Some(other) = other {
			out.push(Verdict { weft, other, outcome: compare(&weft.latency_ci.p50, &other.latency_ci.p50) });
		}
	}
	out
}

/// A rendered block. Markdown and HTML are produced from the same blocks, so they say the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
	/// A heading at level 1–3.
	Heading(u8, String),
	/// A paragraph.
	Para(String),
	/// A bulleted list.
	List(Vec<String>),
	/// A table: header row, then body rows.
	Table(Vec<String>, Vec<Vec<String>>),
}

fn ms(v: u64) -> String {
	format!("{:.3}", ns(v) / 1e6)
}

fn ci(c: &ConfidenceInterval) -> String {
	format!("{} [{}, {}]", ms(c.point_ns), ms(c.lower_ns), ms(c.upper_ns))
}

const fn phase(p: Phase) -> &'static str {
	match p {
		Phase::Load => "load",
		Phase::Interpolate => "interpolate",
		Phase::Downsample => "downsample",
	}
}

/// Bytes per stored point, for a load result that recorded its size.
fn bytes_per_point(r: &ArmResult) -> String {
	match r.storage_bytes {
		#[allow(clippy::cast_precision_loss)]
		Some(bytes) if r.input_points > 0 => format!("{:.2}", bytes as f64 / r.input_points as f64),
		_ => "—".into(),
	}
}

fn verdict_line(v: &Verdict<'_>) -> String {
	let tail = match v.outcome {
		Outcome::Win => format!("{} {:.1}× faster", v.weft.arm.arm, v.ratio()),
		Outcome::Loss => format!("{} {:.1}× faster", v.other.arm.arm, v.ratio()),
		Outcome::Noise => "intervals overlap".to_string(),
	};
	format!("`{}` {}: `{}` p50 {} ms vs `{}` {} ms ({tail}).", v.weft.dataset, phase(v.weft.phase), v.weft.arm.arm, ms(v.weft.latency_ci.p50.point_ns), v.other.arm.arm, ms(v.other.latency_ci.p50.point_ns))
}

/// The report as blocks: header, results, honesty section.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn blocks(report: &XbenchReport) -> Vec<Block> {
	let m = &report.metadata;
	let mut b = vec![Block::Heading(1, "WeftDB cross-engine benchmark".into())];
	let engines = report.engines.iter().map(|e| format!("{} {}", e.engine, e.version)).collect::<Vec<_>>().join(", ");
	b.push(Block::Para(format!("Generated {} on {} ({} {}). Engines: {engines}. Source commit: {}. Seed: {}.", m.generated_at, m.cpu_model.as_deref().unwrap_or("an unreported CPU"), m.os, m.arch, report.source_commit.as_deref().unwrap_or("not recorded"), report.seed)));
	if !report.all_correct() {
		b.push(Block::Para("**At least one result failed its correctness gate. Its timing is not a valid comparison; see the failures below.**".into()));
	}

	b.push(Block::Heading(2, "Results".into()));
	b.push(Block::Para("Latency in milliseconds: p50 with its bootstrap interval, and p95. Accuracy is RMSE against the analytic ground truth, for synthetic datasets. B/pt is the store's size on disk after a load, divided by the points loaded (whole files: indexes and headers included).".into()));
	let rows = report
		.results
		.iter()
		.map(|r| {
			let gate = r.correctness.as_ref().map_or_else(|| "—".into(), |c| if c.passed { "PASS".into() } else { format!("FAIL (max diff {:.3e})", c.max_abs_diff_vs_reference) });
			let rmse = r.accuracy.as_ref().map_or_else(|| "—".into(), |a| format!("{:.4}", a.rmse));
			vec![r.dataset.clone(), phase(r.phase).into(), r.arm.arm.clone(), r.input_points.to_string(), ci(&r.latency_ci.p50), ms(r.latency.p95_ns), r.reps.to_string(), gate, rmse, bytes_per_point(r)]
		})
		.collect();
	b.push(Block::Table(["dataset", "phase", "arm", "input", "p50 [CI]", "p95", "reps", "gate", "RMSE", "B/pt"].map(String::from).to_vec(), rows));

	b.push(Block::Heading(2, "Honesty".into()));
	b.push(Block::Para("Generated from the results above. A WeftDB arm wins or loses only when its p50 interval does not overlap the fastest other engine's; overlapping intervals are within noise.".into()));

	let failures: Vec<String> = report.results.iter().filter(|r| !passed(r)).map(|r| format!("`{}` {} `{}` disagrees with the in-memory reference: {:?}.", r.dataset, phase(r.phase), r.arm.arm, r.correctness)).collect();
	if !failures.is_empty() {
		b.push(Block::Heading(3, "Correctness failures".into()));
		b.push(Block::List(failures));
	}

	let verdicts = verdicts(report);
	for (title, outcome, empty) in [("Where WeftDB wins", Outcome::Win, "Nowhere in this run."), ("Where WeftDB loses", Outcome::Loss, "Nowhere in this run."), ("Within noise", Outcome::Noise, "No comparison in this run was within noise.")] {
		b.push(Block::Heading(3, title.into()));
		let lines: Vec<String> = verdicts.iter().filter(|v| v.outcome == outcome).map(verdict_line).collect();
		if lines.is_empty() {
			b.push(Block::Para(empty.into()));
		} else {
			b.push(Block::List(lines));
		}
	}

	b.push(Block::Heading(3, "Not tested".into()));
	let mut missing = Vec::new();
	for kind in EXPECTED_KINDS {
		let scales: std::collections::BTreeSet<&str> = report.results.iter().filter(|r| r.kind == kind).map(|r| r.scale.as_str()).collect();
		if scales.is_empty() {
			missing.push(format!("The `{}` datasets did not run{}.", kind.as_str(), if kind == DatasetKind::Uci { " (pass `--uci FILE`)" } else { "" }));
		} else if scales.len() < EXPECTED_SCALES {
			missing.push(format!("`{}` ran at {} of {EXPECTED_SCALES} scales ({}).", kind.as_str(), scales.len(), scales.into_iter().collect::<Vec<_>>().join(", ")));
		}
	}
	for arm in EXPECTED_ARMS {
		if !report.results.iter().any(|r| r.arm.arm == arm) {
			missing.push(format!("The `{arm}` arm is not in this run{}.", if arm.starts_with("clickhouse") { " (pass `--clickhouse URL`)" } else { "" }));
		}
	}
	missing.extend(NOT_IN_SCOPE.iter().map(|s| (*s).to_string()));
	b.push(Block::List(missing));

	b.push(Block::Heading(3, "Known limitations".into()));
	let mut limits = vec!["DuckDB has no interpolation function. Its arm is hand-written SQL (two `ASOF JOIN`s and the first/last segment extended), which is what a DuckDB user would write, and it is gated against WeftDB's grid.".to_string(), "WeftDB has no last-observation-carried-forward fill on stored data in 1.0, so no LOCF comparison is made.".to_string(), "Every dataset is a single series; the grid is fixed per dataset, so input size grows with scale while output size does not.".to_string()];
	if report.results.iter().any(|r| r.phase == Phase::Load && is_weft(r)) && report.results.iter().any(|r| r.phase == Phase::Load && !is_weft(r)) {
		limits.push(if report.results.iter().any(|r| r.arm.arm == "weftdb-stored-bulk") { "Load layouts differ by arm: `weftdb-stored` and `weftdb-stored-f64` seal 1,024-row durable segments (the layout the stored read path is measured on), `weftdb-stored-bulk` seals one segment per load, and DuckDB does one bulk append and one CHECKPOINT. `weftdb-stored-bulk` is the like-for-like load comparison.".into() } else { "Load is not like-for-like: WeftDB seals 1,024-row durable segments (the layout its read path is measured on), DuckDB does one bulk append and one CHECKPOINT.".into() });
	}
	if report.results.iter().any(|r| r.arm.arm == "duckdb-decimal") {
		limits.push("`duckdb-decimal` stores the knots exactly as `DECIMAL(18, s)`, but its interpolation line is computed in `DOUBLE` (DuckDB's `DECIMAL × DOUBLE` is `DOUBLE`), as is WeftDB's (splimes computes in `f64`). Neither engine interpolates in exact decimal arithmetic.".into());
	}
	let clickhouse = report.results.iter().any(|r| r.arm.arm.starts_with("clickhouse"));
	if clickhouse {
		limits.push("ClickHouse's own gap filling (`WITH FILL … INTERPOLATE`) carries the previous row's expression forward rather than drawing a line between neighbours, so its arm runs the same hand-written `ASOF JOIN` SQL as DuckDB's, gated the same way.".into());
		limits.push("ClickHouse is a server; the other engines run in-process. Its timings include a loopback HTTP round trip and `RowBinary` encoding both ways, which is what a ClickHouse client pays, and its load is one `INSERT` into a `MergeTree` part with `fsync_after_insert` on.".into());
	}
	if report.results.iter().any(|r| r.arm.arm == "clickhouse-decimal") {
		limits.push("`clickhouse-decimal` stores the knots exactly as `Decimal64(s)`; its interpolation line is computed in `Float64`, since ClickHouse refuses `Decimal × Float64` without a cast.".into());
	}
	b.push(Block::List(limits));

	b.push(Block::Heading(3, "Configuration caveats".into()));
	let mut caveats = vec![format!("Hardware: {} ({} physical / {} logical cores, {} GiB memory). The working directory was on a {} {} disk at {}.", m.cpu_model.as_deref().unwrap_or("CPU not reported"), m.cpu_cores_physical.map_or_else(|| "?".into(), |c| c.to_string()), m.cpu_cores_logical.map_or_else(|| "?".into(), |c| c.to_string()), m.total_memory_bytes.map_or_else(|| "?".into(), |b| format!("{:.0}", ns(b) / f64::from(1u32 << 30))), m.work_disk_kind.as_deref().unwrap_or("unknown"), m.work_disk_file_system.as_deref().unwrap_or("unknown file system"), m.work_disk_mount_point.as_deref().unwrap_or("an unreported mount point"),)];
	if let Some(scratch) = &report.scratch_dir {
		caveats.push(format!("The in-process engines wrote their stores to `{scratch}` (`TMPDIR`), which may be a different disk from the working directory."));
	}
	if clickhouse {
		caveats.push("The ClickHouse server stored its data in its own data directory, which this report cannot see; check the reproduction notes for where it was mounted.".into());
	}
	match report.optimized {
		Some(false) => caveats.push("**This run is a debug build.** Its timings are not comparable with an optimised build's.".into()),
		None => caveats.push("The report does not record whether the binary was an optimised build (a v1 report).".into()),
		Some(true) => {}
	}
	let min_reps = report.results.iter().map(|r| r.reps).min().unwrap_or(0);
	if min_reps < FEW_REPS {
		caveats.push(format!("Some results have only {min_reps} reps, so their intervals are wide and fewer comparisons can separate from noise."));
	}
	b.push(Block::List(caveats));

	b.push(Block::Heading(3, "Reproduction".into()));
	let invocation = if report.invocation.is_empty() { "weft-xbench".to_string() } else { report.invocation.join(" ") };
	b.push(Block::List(vec![format!("Check out commit `{}`.", report.source_commit.as_deref().unwrap_or("(not recorded)")), "Build: `cargo build --release --manifest-path weft-xbench/Cargo.toml` (DuckDB compiles from C++; put `CARGO_TARGET_DIR` on a fast local disk).".into(), format!("Run: `TMPDIR=<scratch on the disk under test> {invocation}`."), "For the ClickHouse arms, a server on the same disk: `docker run -d -p 127.0.0.1:8123:8123 -v <disk under test>/clickhouse:/var/lib/clickhouse clickhouse/clickhouse-server:<version above>`, then `--clickhouse http://127.0.0.1:8123`.".into(), format!("Render: `weft-xbench render <xbench.json> --out <dir>`. The synthetic datasets are regenerated from seed {}.", report.seed)]));
	b
}

fn md_cell(s: &str) -> String {
	s.replace('|', "\\|").replace('\n', " ")
}

/// Render blocks as Markdown.
#[must_use]
pub fn to_markdown(blocks: &[Block]) -> String {
	let mut out = String::new();
	for block in blocks {
		match block {
			Block::Heading(level, text) => {
				let _ = writeln!(out, "{} {text}\n", "#".repeat(usize::from(*level)));
			}
			Block::Para(text) => {
				let _ = writeln!(out, "{text}\n");
			}
			Block::List(items) => {
				for item in items {
					let _ = writeln!(out, "- {item}");
				}
				out.push('\n');
			}
			Block::Table(head, rows) => {
				let _ = writeln!(out, "| {} |", head.iter().map(|h| md_cell(h)).collect::<Vec<_>>().join(" | "));
				let _ = writeln!(out, "|{}|", head.iter().map(|_| " --- ").collect::<Vec<_>>().join("|"));
				for row in rows {
					let _ = writeln!(out, "| {} |", row.iter().map(|c| md_cell(c)).collect::<Vec<_>>().join(" | "));
				}
				out.push('\n');
			}
		}
	}
	out
}

fn escape(s: &str) -> String {
	s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Inline `code` and **bold** spans, the only Markdown the blocks use, as HTML.
fn inline(s: &str) -> String {
	let escaped = escape(s);
	let mut out = String::with_capacity(escaped.len());
	let (mut code, mut bold) = (false, false);
	let mut chars = escaped.chars().peekable();
	while let Some(c) = chars.next() {
		if c == '`' {
			out.push_str(if code { "</code>" } else { "<code>" });
			code = !code;
		} else if c == '*' && !code && chars.peek() == Some(&'*') {
			chars.next();
			out.push_str(if bold { "</strong>" } else { "<strong>" });
			bold = !bold;
		} else {
			out.push(c);
		}
	}
	out
}

/// Render blocks as a self-contained HTML page (inline styles, no scripts, no external assets).
#[must_use]
pub fn to_html(blocks: &[Block]) -> String {
	let mut body = String::new();
	for block in blocks {
		match block {
			Block::Heading(level, text) => {
				let _ = writeln!(body, "<h{level}>{}</h{level}>", inline(text));
			}
			Block::Para(text) => {
				let _ = writeln!(body, "<p>{}</p>", inline(text));
			}
			Block::List(items) => {
				body.push_str("<ul>\n");
				for item in items {
					let _ = writeln!(body, "<li>{}</li>", inline(item));
				}
				body.push_str("</ul>\n");
			}
			Block::Table(head, rows) => {
				body.push_str("<table>\n<thead><tr>");
				for h in head {
					let _ = write!(body, "<th>{}</th>", inline(h));
				}
				body.push_str("</tr></thead>\n<tbody>\n");
				for row in rows {
					body.push_str("<tr>");
					for c in row {
						let _ = write!(body, "<td>{}</td>", inline(c));
					}
					body.push_str("</tr>\n");
				}
				body.push_str("</tbody>\n</table>\n");
			}
		}
	}
	format!("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>WeftDB cross-engine benchmark</title>\n<style>\nbody{{font:15px/1.5 system-ui,sans-serif;max-width:72rem;margin:2rem auto;padding:0 1rem;color:#1b1b1b;background:#fff}}\ntable{{border-collapse:collapse;width:100%;font-size:13px}}th,td{{border:1px solid #ccc;padding:.25rem .5rem;text-align:left}}th{{background:#f3f3f3}}\ncode{{font:13px ui-monospace,monospace;background:#f3f3f3;padding:0 .2rem}}\n@media (prefers-color-scheme:dark){{body{{color:#e8e8e8;background:#161616}}th,code{{background:#262626}}th,td{{border-color:#444}}}}\n</style>\n</head>\n<body>\n{body}</body>\n</html>\n")
}

#[cfg(test)]
mod tests {
	use weft_bench::report::RunMetadata;

	use super::*;
	use crate::{latency, ArmInfo, Correctness, XBENCH_SCHEMA_VERSION};

	fn result(dataset: &str, arm: &str, phase: Phase, samples: &[u64], passed: bool) -> ArmResult {
		let (latency, latency_ci) = latency(samples, 1);
		let correctness = (phase == Phase::Interpolate).then_some(Correctness { expected_points: 3, actual_points: 3, values_finite: true, max_abs_diff_vs_reference: if passed { 0.0 } else { 1.0 }, tolerance: 1e-9, passed });
		ArmResult { dataset: dataset.into(), kind: DatasetKind::Mcar, scale: "tiny".into(), input_points: 10, output_points: 3, missingness_fraction: 0.2, arm: ArmInfo { arm: arm.into(), engine: "e".into(), value_type: "v".into(), method: "m".into() }, phase, reps: samples.len(), latency, latency_ci, correctness, accuracy: None, storage_bytes: None }
	}

	fn report(results: Vec<ArmResult>) -> XbenchReport {
		XbenchReport { schema_version: XBENCH_SCHEMA_VERSION, metadata: RunMetadata::capture("2026-10-29T00:00:00Z".into()), engines: vec![], seed: 7, source_commit: Some("abc123".into()), invocation: vec!["weft-xbench".into(), "--scales".into(), "tiny".into()], scratch_dir: Some("/mnt/weftbench/tmp".into()), optimized: Some(true), results }
	}

	const FAST: [u64; 12] = [100, 101, 99, 100, 102, 98, 100, 101, 99, 100, 100, 101];
	const SLOW: [u64; 12] = [500, 505, 495, 500, 510, 490, 500, 505, 495, 500, 500, 505];

	#[test]
	fn disjoint_intervals_decide_and_overlapping_ones_are_noise() {
		let r = report(vec![result("a", "weftdb-memory", Phase::Interpolate, &FAST, true), result("a", "duckdb-double", Phase::Interpolate, &SLOW, true), result("b", "weftdb-memory", Phase::Interpolate, &SLOW, true), result("b", "duckdb-double", Phase::Interpolate, &FAST, true), result("c", "weftdb-memory", Phase::Interpolate, &FAST, true), result("c", "duckdb-double", Phase::Interpolate, &FAST, true)]);
		let outcomes: Vec<(String, Outcome)> = verdicts(&r).iter().map(|v| (v.weft.dataset.clone(), v.outcome)).collect();
		assert_eq!(outcomes, vec![("a".into(), Outcome::Win), ("b".into(), Outcome::Loss), ("c".into(), Outcome::Noise)]);
	}

	#[test]
	fn a_failed_gate_is_listed_first_and_never_compared() {
		let r = report(vec![result("a", "weftdb-memory", Phase::Interpolate, &FAST, false), result("a", "duckdb-double", Phase::Interpolate, &SLOW, true)]);
		assert!(verdicts(&r).is_empty(), "a failing WeftDB arm cannot win");
		let md = to_markdown(&blocks(&r));
		assert!(md.contains("failed its correctness gate"));
		assert!(md.find("### Correctness failures").expect("listed") < md.find("### Where WeftDB wins").expect("present"));
	}

	#[test]
	fn the_honesty_section_names_what_did_not_run_and_the_load_caveat() {
		let r = report(vec![result("a", "weftdb-stored", Phase::Load, &SLOW[..5], true), result("a", "duckdb-double", Phase::Load, &FAST[..5], true)]);
		let md = to_markdown(&blocks(&r));
		for needle in ["The `uci` datasets did not run (pass `--uci FILE`)", "`mcar` ran at 1 of 3 scales", "The `duckdb-decimal` arm is not in this run", "Load is not like-for-like", "kdb+ and AVEVA PI", "Where WeftDB loses", "Some results have only 5 reps"] {
			assert!(md.contains(needle), "missing {needle:?} in\n{md}");
		}
		assert!(md.contains("Nowhere in this run."), "no win must say so rather than vanish");
	}

	#[test]
	fn clickhouse_is_named_as_missing_or_carries_its_server_caveats() {
		let without = to_markdown(&blocks(&report(vec![result("a", "weftdb-stored", Phase::Load, &SLOW[..5], true)])));
		assert!(without.contains("The `clickhouse-double` arm is not in this run (pass `--clickhouse URL`)"), "{without}");
		assert!(!without.contains("ClickHouse is a server"), "no server caveat without a ClickHouse arm");
		let with = to_markdown(&blocks(&report(vec![result("a", "weftdb-stored", Phase::Load, &SLOW[..5], true), result("a", "clickhouse-decimal", Phase::Load, &FAST[..5], true)])));
		for needle in ["ClickHouse is a server", "`WITH FILL … INTERPOLATE`", "`clickhouse-decimal` stores the knots exactly", "its own data directory"] {
			assert!(with.contains(needle), "missing {needle:?} in\n{with}");
		}
	}

	#[test]
	fn a_debug_build_is_flagged() {
		let mut r = report(vec![]);
		r.optimized = Some(false);
		assert!(to_markdown(&blocks(&r)).contains("debug build"));
	}

	#[test]
	fn html_is_escaped_and_self_contained() {
		let html = to_html(&[Block::Para("a <b> & `x<y>` **bold**".into()), Block::Table(vec!["h".into()], vec![vec!["<script>".into()]])]);
		assert!(html.contains("a &lt;b&gt; &amp; <code>x&lt;y&gt;</code> <strong>bold</strong>"));
		assert!(html.contains("<td>&lt;script&gt;</td>"));
		assert!(!html.contains("<script") && !html.contains("http"), "no scripts or external assets");
	}

	#[test]
	fn markdown_tables_escape_pipes() {
		let md = to_markdown(&[Block::Table(vec!["a|b".into()], vec![vec!["c|d".into()]])]);
		assert!(md.contains("| a\\|b |") && md.contains("| c\\|d |"));
	}
}
