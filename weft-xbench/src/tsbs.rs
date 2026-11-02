//! TSBS data as a benchmark input (#141).
//!
//! The Time Series Benchmark Suite's generator (`tsbs_generate_data --use-case=cpu-only
//! --format=influx`) writes InfluxDB line protocol: one `cpu` line per host every `--log-interval`,
//! tagged `hostname=host_N` and nine others, with ten integer `usage_*` fields and a nanosecond
//! timestamp, every host at each instant before the next instant. This module reads that file and
//! keeps one host's field as a series, which the harness then runs through the same load,
//! interpolation and downsample phases as every other dataset.
//!
//! It is TSBS *data*, not TSBS's query suite: those queries group many series by tags, which
//! WeftDB stores once tagged ingest (TAG-11) lands, and TAG-12 owns them.

use std::{collections::BTreeSet, io::BufRead, path::Path};

use splimes::Point;
use weft_line_protocol::{SeriesKey, TimestampPrecision};

/// The measurement TSBS's `cpu-only` use case writes.
pub const MEASUREMENT: &str = "cpu";

/// The host whose series the harness keeps unless told otherwise.
pub const DEFAULT_HOST: &str = "host_0";

/// The field the harness keeps: user CPU, the one TSBS's own `cpu-max-all` and `high-cpu` queries
/// lead with.
pub const DEFAULT_FIELD: &str = "usage_user";

/// One host's field from a TSBS file.
#[derive(Debug, Clone)]
pub struct TsbsSeries {
	/// The `hostname` tag kept.
	pub host: String,
	/// The field kept.
	pub field: String,
	/// The host's readings, in time order.
	pub points: Vec<Point>,
	/// Distinct `hostname` tags seen in the lines read (the file's `--scale`, once a full instant
	/// has been read).
	pub hosts: usize,
}

/// Read `host`'s `field` from the TSBS file at `path`, up to `span_secs` after its first reading.
///
/// # Errors
///
/// See [`load_from_reader`].
pub fn load(path: &Path, host: &str, field: &str, span_secs: i64) -> anyhow::Result<TsbsSeries> {
	let file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
	load_from_reader(std::io::BufReader::new(file), host, field, span_secs).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// The reader half of [`load`]. Stops at the first line past the span, since TSBS writes in time
/// order; everything before it is parsed, so a malformed line is refused wherever it is.
///
/// # Errors
///
/// A malformed line, a `cpu` line for `host` without a timestamp or a numeric `field`, a second
/// tag set for `host`, readings for `host` that do not strictly increase, or fewer than two of
/// them.
pub fn load_from_reader(reader: impl BufRead, host: &str, field: &str, span_secs: i64) -> anyhow::Result<TsbsSeries> {
	let mut points: Vec<Point> = Vec::new();
	let mut hosts = BTreeSet::new();
	let mut series: Option<SeriesKey> = None;
	let mut end_ns = None;
	for (idx, line) in reader.lines().enumerate() {
		let line_no = idx + 1;
		let line = line.map_err(|e| anyhow::anyhow!("line {line_no}: {e}"))?;
		let records = weft_line_protocol::parse(&line).map_err(|e| anyhow::anyhow!("line {line_no}: {e}"))?;
		let Some(record) = records.into_iter().next() else { continue };
		if let (Some(end), Some(ts)) = (end_ns, record.timestamp) {
			if ts > end {
				break;
			}
		}
		if record.measurement != MEASUREMENT {
			continue;
		}
		let Some(name) = record.tag("hostname") else { continue };
		hosts.insert(name.to_string());
		if name != host {
			continue;
		}
		let key = record.series_key();
		match &series {
			None => series = Some(key),
			Some(first) if *first != key => anyhow::bail!("line {line_no}: {host} changed its tag set to {}", record.describe_series()),
			Some(_) => {}
		}
		let point = record.to_point(field, TimestampPrecision::Nanoseconds).ok_or_else(|| anyhow::anyhow!("line {line_no}: {host} has no timestamp or numeric {field:?}"))?;
		if let Some(last) = points.last() {
			anyhow::ensure!(point.timestamp > last.timestamp, "line {line_no}: {host}'s reading at {} does not follow {}", point.timestamp, last.timestamp);
		} else {
			let first = point.timestamp.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("line {line_no}: instant out of range"))?;
			end_ns = Some(first.saturating_add(span_secs.saturating_mul(1_000_000_000)));
		}
		points.push(point);
	}
	anyhow::ensure!(points.len() >= 2, "{host} has {} {MEASUREMENT} readings with {field:?}; at least two are needed", points.len());
	Ok(TsbsSeries { host: host.to_string(), field: field.to_string(), points, hosts: hosts.len() })
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Two hosts at three instants, ten seconds apart, as `tsbs_generate_data` writes them (tags
	/// trimmed).
	const SAMPLE: &str = "\
cpu,hostname=host_0,region=eu-central-1 usage_user=58i,usage_system=2i 1451606400000000000
cpu,hostname=host_1,region=us-west-1 usage_user=84i,usage_system=11i 1451606400000000000
cpu,hostname=host_0,region=eu-central-1 usage_user=59i,usage_system=4i 1451606410000000000
cpu,hostname=host_1,region=us-west-1 usage_user=85i,usage_system=13i 1451606410000000000
cpu,hostname=host_0,region=eu-central-1 usage_user=61i,usage_system=3i 1451606420000000000
cpu,hostname=host_1,region=us-west-1 usage_user=83i,usage_system=12i 1451606420000000000
";

	fn values(series: &TsbsSeries) -> Vec<String> {
		series.points.iter().map(|p| p.value.to_string()).collect()
	}

	#[test]
	fn keeps_one_hosts_field_and_counts_the_hosts() {
		let series = load_from_reader(SAMPLE.as_bytes(), "host_1", "usage_user", 3600).expect("reads");
		assert_eq!(values(&series), ["84", "85", "83"]);
		assert_eq!(series.hosts, 2);
		assert_eq!(series.points[0].timestamp.timestamp(), 1_451_606_400);
	}

	#[test]
	fn stops_at_the_span() {
		let series = load_from_reader(SAMPLE.as_bytes(), DEFAULT_HOST, DEFAULT_FIELD, 10).expect("reads");
		assert_eq!(values(&series), ["58", "59"]);
	}

	#[test]
	fn refuses_what_is_not_one_clean_series() {
		let unknown = load_from_reader(SAMPLE.as_bytes(), "host_9", DEFAULT_FIELD, 3600).expect_err("no such host");
		assert!(unknown.to_string().contains("host_9 has 0"), "{unknown}");
		let backwards = "cpu,hostname=host_0 usage_user=1i 20\ncpu,hostname=host_0 usage_user=2i 10\n";
		assert!(load_from_reader(backwards.as_bytes(), DEFAULT_HOST, DEFAULT_FIELD, 3600).is_err());
		let retagged = "cpu,hostname=host_0,rack=1 usage_user=1i 10\ncpu,hostname=host_0,rack=2 usage_user=2i 20\n";
		assert!(load_from_reader(retagged.as_bytes(), DEFAULT_HOST, DEFAULT_FIELD, 3600).expect_err("two tag sets").to_string().contains("changed its tag set"));
		assert!(load_from_reader(&b"cpu,hostname=host_0 usage_user=\n"[..], DEFAULT_HOST, DEFAULT_FIELD, 3600).is_err());
	}
}
