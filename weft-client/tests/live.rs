//! The client against a real `weft-server`: the wire contract.
//!
//! Run: `WEFT_SERVER_BIN=target/debug/weft-server cargo test -p weftdb-client --test live`.
//! Without `WEFT_SERVER_BIN`, or with it naming no file, every test here passes without
//! running (the unit and fake-server tests still run). Each test starts its own server
//! on a free loopback port over a throwaway store and run directory, with a clean
//! environment, and stops it on drop.
#![recursion_limit = "1024"]
#![forbid(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions)]

use std::{
	path::PathBuf, process::{Child, Command, Stdio}, str::FromStr as _, time::{Duration, Instant}
};

use chrono::{DateTime, Utc};
use futures::TryStreamExt as _;
use weftdb_client::{AspectDeclaration, BigDecimal, Client, ComputePoint, Downsample, ErrorKind, Format, IlpOptions, Interpolate, Page, PointKind, Spline, StoredDownsample, WriteOptions};

const MANIFEST: &[u8] = br#"{"schema_version":23,"generated_at":"2026-11-03T00:00:00Z","claim":"sdk-live-rust"}"#;

struct Server {
	child: Child,
	client: Client,
	dir: tempfile::TempDir,
}

impl Drop for Server {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

/// A running server, or `None` when `WEFT_SERVER_BIN` names no binary.
async fn server() -> Option<Server> {
	let bin = PathBuf::from(std::env::var_os("WEFT_SERVER_BIN")?);
	if !bin.is_file() {
		eprintln!("WEFT_SERVER_BIN={} is not a file; skipping the live contract", bin.display());
		return None;
	}
	let dir = tempfile::tempdir().unwrap();
	let runs = dir.path().join("runs").join("nightly");
	std::fs::create_dir_all(&runs).unwrap();
	std::fs::write(runs.join("interp__weftdb.manifest.json"), MANIFEST).unwrap();
	let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
	let log = std::fs::File::create(dir.path().join("server.log")).unwrap();
	let child = Command::new(&bin).env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("HOME", dir.path()).env("WEFT_SERVER_ADDR", format!("127.0.0.1:{port}")).env("WEFT_SEGMENT_STORE_ROOT", dir.path().join("store")).env("WEFT_BENCH_RUNS_DIR", dir.path().join("runs")).env("WEFT_GPU_CALIBRATE", "0").env("RUST_LOG", "warn").stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
	let client = Client::builder(format!("http://127.0.0.1:{port}")).timeout(Duration::from_secs(20)).build().unwrap();
	let mut server = Server { child, client, dir };
	let deadline = Instant::now() + Duration::from_secs(60);
	while server.client.health().await.is_err() {
		let exited = server.child.try_wait().unwrap();
		assert!(exited.is_none() && Instant::now() < deadline, "weft-server did not come up ({exited:?}); its log:\n{}", std::fs::read_to_string(server.dir.path().join("server.log")).unwrap_or_default());
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	Some(server)
}

fn at(text: &str) -> DateTime<Utc> {
	DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
}

fn decimal(text: &str) -> BigDecimal {
	BigDecimal::from_str(text).unwrap()
}

#[tokio::test]
async fn probes() {
	let Some(server) = server().await else { return };
	assert!(server.client.health().await.unwrap().get("status").is_some());
	let ready = server.client.ready().await.unwrap();
	assert_eq!(ready["ready"], true, "{ready}");
	assert_eq!(ready["segment_store"], true, "{ready}");
	assert!(server.client.metrics().await.unwrap().contains("weft_"));
	assert!(server.client.profile().await.unwrap().is_object());
}

#[tokio::test]
async fn a_series_round_trips_losslessly() {
	let Some(server) = server().await else { return };
	let client = &server.client;
	let declaration = AspectDeclaration::new("price", "scaled_i64", "millis").scale(2).value_tolerance("0").unwrap();
	let info = client.declare_aspect(&declaration).await.unwrap();
	assert_eq!((info.scale, info.timestamp_unit.as_str()), (Some(2), "millis"));
	client.declare_aspect(&declaration).await.expect("an identical declaration is a no-op, not a conflict");

	let rows = [(1_000, decimal("1.25")), (2_000, decimal("2.50")), (3_000, decimal("3"))];
	let written = client.write_points("price", rows.clone(), &WriteOptions::idempotent("rust-live-1")).await.unwrap();
	assert_eq!((written.row_count, written.replayed), (3, false));
	let replay = client.write_points("price", rows, &WriteOptions::idempotent("rust-live-1")).await.unwrap();
	assert!(replay.replayed, "the same key answers the first commit instead of sealing twice");

	let streamed: Vec<_> = client.points_stream("price", 0, 9_999, 2).try_collect().await.unwrap();
	let got: Vec<(i64, String)> = streamed.iter().map(|p| (p.timestamp, p.value.as_ref().unwrap().to_string())).collect();
	assert_eq!(got, [(1_000, "1.25".into()), (2_000, "2.50".into()), (3_000, "3.00".into())], "the aspect's scale, every digit");
	let first = client.points("price", 0, 9_999, &Page::first(2)).await.unwrap();
	assert_eq!((first.total, first.count, first.next_cursor.is_some()), (Some(3), 2, true));

	assert_eq!(client.at("price", 2_000).await.unwrap().value, Some(decimal("2.50")));
	assert!(!client.at("price", 2_500).await.unwrap().found);
	let multi = client.at_multi("price", &[1_000, 1_500, 3_000]).await.unwrap();
	assert_eq!(multi.iter().map(|p| p.found).collect::<Vec<_>>(), [true, false, true]);
	let band: Vec<_> = client.value_points_stream("price", "2", "4", 10).unwrap().try_collect().await.unwrap();
	assert_eq!(band.iter().map(|p| p.timestamp).collect::<Vec<_>>(), [2_000, 3_000]);
	let csv = String::from_utf8(client.range("price", 0, 9_999, Format::Csv).await.unwrap()).unwrap();
	assert!(csv.starts_with("timestamp"), "{csv}");
	assert_ne!(client.value_range("price", "0", "9", Format::Arrow).await.unwrap().len(), 0, "the value range is not empty");

	let down = client.stored_downsample("price", &StoredDownsample::new(0, 9_999, "seconds", ["avg", "max"])).await.unwrap();
	assert_eq!(down.buckets, 3);
	assert_eq!(client.aspect_schema("price").await.unwrap().scale, Some(2));
	assert!(client.aspects().await.unwrap().iter().any(|a| a.name == "price"));
	assert_eq!(client.aspect_stats("price").await.unwrap().total_rows, 3);
	assert!(client.stats().await.unwrap().aspect_count >= 1);
	assert!(client.catalog().await.unwrap().is_object());
}

#[tokio::test]
async fn the_other_ingest_formats_seal() {
	let Some(server) = server().await else { return };
	let client = &server.client;
	client.declare_aspect(&AspectDeclaration::new("load", "bigdecimal_text", "seconds")).await.unwrap();
	let csv = client.write_csv("load", "timestamp,value\n1,0.5\n2,0.75\n", &WriteOptions::default()).await.unwrap();
	assert_eq!(csv.row_count, 2);
	let ilp = client.write_ilp("load", "load", "cpu load=1.5 3\ncpu load=2 4\n", &WriteOptions::default().precision("s").measurement("cpu")).await.unwrap();
	assert_eq!(ilp.row_count, 2);
	let all: Vec<_> = client.points_stream("load", 0, 10, 100).try_collect().await.unwrap();
	assert_eq!(all.len(), 4);
}

#[tokio::test]
async fn the_storage_boundary_refuses_what_it_cannot_hold() {
	let Some(server) = server().await else { return };
	let client = &server.client;
	client.declare_aspect(&AspectDeclaration::new("exact", "scaled_i64", "millis").scale(2)).await.unwrap();
	let refused = client.write_points("exact", [(1, "1.234")], &WriteOptions::default()).await.unwrap_err();
	assert_eq!((refused.kind(), refused.code()), (ErrorKind::BadRequest, Some("value_not_representable")), "{refused}");
	let missing = client.aspect_schema("never-declared").await.unwrap_err();
	assert_eq!((missing.kind(), missing.code()), (ErrorKind::NotFound, Some("aspect_not_found")));
	assert!(missing.request_id().is_some_and(|id| !id.is_empty()), "every refusal names its request");
	let traversal = client.aspect_schema("../etc").await.unwrap_err();
	assert_ne!(traversal.kind(), ErrorKind::Transport, "a slash in a name is one segment, answered by the server: {traversal}");
}

#[tokio::test]
async fn compute() {
	let Some(server) = server().await else { return };
	let client = &server.client;
	let points = [ComputePoint::new(at("1970-01-01T00:00:00Z"), 0.0), ComputePoint::new(at("1970-01-01T00:00:30Z"), 30.0), ComputePoint::new(at("1970-01-01T00:01:00Z"), 60.0)];
	let answer = client.interpolate(&Interpolate::new(points).spline(Spline::Linear)).await.unwrap();
	assert_eq!(answer.output_points, 61);
	assert!(answer.points.iter().all(|p| matches!(p.kind, PointKind::Raw | PointKind::Interpolated)));
	assert!((answer.points[15].value - 15.0).abs() < 1e-9);
	let point = client.interpolate_point(&points, at("1970-01-01T00:00:15Z"), Spline::Linear).await.unwrap();
	assert!((point.value - 15.0).abs() < 1e-9);
	assert_eq!(client.downsample(&Downsample::new(points, ["avg"]).resolution("minutes")).await.unwrap().buckets, 2);
	let ilp = "cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n";
	let options = IlpOptions::field("load").precision("s").spline(Spline::Linear).resolution("seconds");
	assert_eq!(client.interpolate_ilp(ilp, &options).await.unwrap().output_points, 61);
	let csv = client.interpolate_as(&Interpolate::new(points).spline(Spline::Linear), Format::Csv).await.unwrap();
	assert!(String::from_utf8(csv).unwrap().lines().count() > 60);
}

#[cfg(feature = "arrow")]
#[tokio::test]
async fn columnar_answers_decode() {
	let Some(server) = server().await else { return };
	let points = [ComputePoint::new(at("1970-01-01T00:00:00Z"), 0.0), ComputePoint::new(at("1970-01-01T00:01:00Z"), 60.0)];
	let request = Interpolate::new(points).spline(Spline::Linear);
	let arrow = weftdb_client::decode_arrow(&server.client.interpolate_as(&request, Format::Arrow).await.unwrap()).unwrap();
	assert_eq!(arrow.iter().map(arrow_array::RecordBatch::num_rows).sum::<usize>(), 61);
	let parquet = weftdb_client::decode_parquet(&server.client.interpolate_as(&request, Format::Parquet).await.unwrap()).unwrap();
	assert_eq!(parquet.iter().map(arrow_array::RecordBatch::num_rows).sum::<usize>(), 61);
}

#[tokio::test]
async fn bench_runs() {
	use sha2::Digest as _;
	let Some(server) = server().await else { return };
	let id = format!("{:x}", sha2::Sha256::digest(MANIFEST));
	let runs = server.client.bench_runs().await.unwrap();
	assert_eq!(runs.iter().map(|r| (r.id.as_str(), r.path.as_str(), r.claim.as_deref())).collect::<Vec<_>>(), [(id.as_str(), "nightly/interp__weftdb.manifest.json", Some("sdk-live-rust"))]);
	assert_eq!(server.client.bench_run(&id).await.unwrap(), MANIFEST);
	let missing = server.client.bench_run(&format!("{:x}", sha2::Sha256::digest(b"edited"))).await.unwrap_err();
	assert_eq!(missing.code(), Some("run_not_found"));
}
