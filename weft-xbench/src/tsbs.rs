//! TSBS data as a benchmark input (#141).
//!
//! The Time Series Benchmark Suite's generator (`tsbs_generate_data --use-case=cpu-only
//! --format=influx`) writes InfluxDB line protocol: one `cpu` line per host every `--log-interval`,
//! tagged `hostname=host_N` and nine others, with ten integer `usage_*` fields and a nanosecond
//! timestamp, every host at each instant before the next instant. This module reads that file and
//! keeps one host's field as a series, which the harness then runs through the same load,
//! interpolation and downsample phases as every other dataset.
//!
//! [`load`] keeps one host's field. [`load_cpu`] keeps every host's ten `usage_*` fields, and
//! [`TsbsCpu`] answers TSBS's tag-grouped devops queries over them ([`TsbsQuery`]), as the
//! reference every engine's answer to the same query is checked against (TAG-12, #341). The
//! answers follow the SQL TSBS generates for `TimescaleDB`
//! (`cmd/tsbs_generate_queries/databases/timescaledb/devops.go`), not the README's prose: a
//! `single-groupby` buckets by **one minute**, which the README calls "every 5 mins".

use std::{
	collections::{BTreeMap, BTreeSet}, io::BufRead, path::Path
};

use splimes::Point;
use weft_line_protocol::{FieldValue, SeriesKey, TimestampPrecision};

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

/// The ten `cpu` fields TSBS writes, in the order its query generator takes the first `N` of
/// (`cpuMetrics` in `cmd/tsbs_generate_queries/uses/devops/common.go`).
pub const CPU_METRICS: [&str; 10] = ["usage_user", "usage_system", "usage_idle", "usage_nice", "usage_iowait", "usage_irq", "usage_softirq", "usage_steal", "usage_guest", "usage_guest_nice"];

/// Nanoseconds in the one-minute bucket a `single-groupby` groups by.
const MINUTE_NS: i64 = 60_000_000_000;

/// One hour in nanoseconds: `cpu-max-all`'s bucket (`time_bucket('3600 seconds', time)`).
const HOUR_NS: i64 = 3_600_000_000_000;

/// `cpu-max-all`'s window in hours: TSBS's `MaxAllDuration` (`uses/devops/common.go`).
pub const MAX_ALL_HOURS: u32 = 8;

/// `high-cpu`'s window in hours: TSBS's `HighCPUDuration` (`uses/devops/common.go`).
pub const HIGH_CPU_HOURS: u32 = 12;

/// `high-cpu`'s threshold: `usage_user > 90.0`.
pub const HIGH_CPU_ABOVE: i64 = 90;

/// `groupby-orderby-limit`'s `LIMIT`.
pub const ORDERBY_LIMIT: usize = 5;

/// One `cpu` line: its instant and its ten fields in [`CPU_METRICS`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuReading {
	/// The line's timestamp, in nanoseconds since the Unix epoch.
	pub ts_ns: i64,
	/// The ten `usage_*` fields, in [`CPU_METRICS`] order.
	pub values: [i64; 10],
}

/// Every host's `cpu` readings from a TSBS `cpu-only` file, keyed by the `hostname` tag.
///
/// The fields are kept as the integers TSBS writes (`58i`): a float, unsigned, boolean or string
/// field is refused, not converted, so a reference answer is exact by construction.
#[derive(Debug, Clone, Default)]
pub struct TsbsCpu {
	/// Each host's readings, in time order. A `BTreeMap`, so hosts iterate in byte order of their
	/// names (`host_10` before `host_2`), which is what `ORDER BY hostname` gives under the C
	/// collation.
	pub hosts: BTreeMap<String, Vec<CpuReading>>,
}

/// The TSBS devops queries [`TsbsCpu`] answers, named as `tsbs_generate_queries --query-type`
/// names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsbsQuery {
	/// `single-groupby-<metrics>-<hosts>-<hours>`: the MAX of the first `metrics` CPU fields over
	/// `hosts` hosts, per one-minute bucket, over `hours` hours.
	SingleGroupby {
		/// How many of [`CPU_METRICS`], from the first.
		metrics: usize,
		/// How many hosts the query names.
		hosts: usize,
		/// The window, in hours.
		hours: u32,
	},
	/// `cpu-max-all-<hosts>`: the MAX of all ten CPU fields over `hosts` hosts, per one-hour
	/// bucket, over `hours` hours (TSBS's `MaxAllCPU`; its generator registers 1 and 8 hosts over
	/// `MaxAllDuration`, eight hours, and 32 hosts over 24 hours as `cpu-max-all-32-24`).
	CpuMaxAll {
		/// How many hosts the query names.
		hosts: usize,
		/// The window, in hours.
		hours: u32,
	},
	/// `high-cpu-<hosts>`: every reading of `hosts` hosts (all of them when `hosts` is 0,
	/// `high-cpu-all`) whose `usage_user` is above 90, over `hours` hours (TSBS's
	/// `HighCPUForHosts`, a `SELECT *` over `HighCPUDuration`, twelve hours).
	HighCpu {
		/// How many hosts the query names; 0 is every host.
		hosts: usize,
		/// The window, in hours.
		hours: u32,
	},
	/// `groupby-orderby-limit`: the MAX of `usage_user` over every host, per one-minute bucket,
	/// for the five latest buckets before a random instant (TSBS's `GroupByOrderByLimit`: no
	/// lower time bound, `ORDER BY minute DESC LIMIT 5`).
	GroupbyOrderbyLimit,
	/// `lastpoint`: the latest reading of every host.
	Lastpoint,
}

/// One row of a `cpu-max-all` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HourMax {
	/// The bucket's start, in nanoseconds since the Unix epoch, a multiple of one hour.
	pub hour_ns: i64,
	/// The MAX of each of the ten [`CPU_METRICS`] over the bucket's rows of the named hosts.
	pub max: Vec<i64>,
}

/// One row of a `single-groupby` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinuteMax {
	/// The bucket's start, in nanoseconds since the Unix epoch, a multiple of one minute.
	pub minute_ns: i64,
	/// The MAX of each asked-for metric over the bucket's rows of the named hosts.
	pub max: Vec<i64>,
}

impl TsbsQuery {
	/// The query types TSBS defines that this module answers, for `single-groupby` only the
	/// variants TSBS's own generator registers (`1-1-1`, `1-1-12`, `1-8-1`, `5-1-1`, `5-1-12`,
	/// `5-8-1`), then `cpu-max-all-1` and `cpu-max-all-8`, then `high-cpu-1` and `high-cpu-all`,
	/// then `groupby-orderby-limit`, then `lastpoint`. `cpu-max-all-32-24` is left out: it names
	/// 32 hosts, and the reference file holds ten.
	pub const ALL: [Self; 12] = [Self::SingleGroupby { metrics: 1, hosts: 1, hours: 1 }, Self::SingleGroupby { metrics: 1, hosts: 1, hours: 12 }, Self::SingleGroupby { metrics: 1, hosts: 8, hours: 1 }, Self::SingleGroupby { metrics: 5, hosts: 1, hours: 1 }, Self::SingleGroupby { metrics: 5, hosts: 1, hours: 12 }, Self::SingleGroupby { metrics: 5, hosts: 8, hours: 1 }, Self::CpuMaxAll { hosts: 1, hours: MAX_ALL_HOURS }, Self::CpuMaxAll { hosts: 8, hours: MAX_ALL_HOURS }, Self::HighCpu { hosts: 1, hours: HIGH_CPU_HOURS }, Self::HighCpu { hosts: 0, hours: HIGH_CPU_HOURS }, Self::GroupbyOrderbyLimit, Self::Lastpoint];

	/// The `--query-type` name.
	#[must_use]
	pub fn name(self) -> String {
		match self {
			Self::SingleGroupby { metrics, hosts, hours } => format!("single-groupby-{metrics}-{hosts}-{hours}"),
			Self::CpuMaxAll { hosts, hours: MAX_ALL_HOURS } => format!("cpu-max-all-{hosts}"),
			Self::CpuMaxAll { hosts, hours } => format!("cpu-max-all-{hosts}-{hours}"),
			Self::HighCpu { hosts: 0, hours: HIGH_CPU_HOURS } => "high-cpu-all".to_string(),
			Self::HighCpu { hosts, hours: HIGH_CPU_HOURS } => format!("high-cpu-{hosts}"),
			Self::HighCpu { hosts, hours } => format!("high-cpu-{hosts}-{hours}h"),
			Self::GroupbyOrderbyLimit => "groupby-orderby-limit".to_string(),
			Self::Lastpoint => "lastpoint".to_string(),
		}
	}

	/// Parse a `--query-type` name.
	///
	/// # Errors
	///
	/// A name that is not in [`Self::ALL`].
	pub fn parse(name: &str) -> anyhow::Result<Self> {
		Self::ALL.into_iter().find(|q| q.name() == name).ok_or_else(|| anyhow::anyhow!("unknown or unsupported TSBS query type {name:?}; supported: {}", Self::ALL.map(Self::name).join(", ")))
	}
}

impl TsbsCpu {
	/// How many readings across all hosts.
	#[must_use]
	pub fn len(&self) -> usize {
		self.hosts.values().map(Vec::len).sum()
	}

	/// Whether no host has a reading.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.hosts.values().all(Vec::is_empty)
	}

	/// `single-groupby`'s answer: for each one-minute bucket in `[start_ns, end_ns)` that holds a
	/// reading of one of `hosts`, the MAX of each of the first `metrics` [`CPU_METRICS`], in bucket
	/// order. Buckets with no reading are absent, as `GROUP BY` leaves them.
	///
	/// TSBS picks the hosts and the window at random from its own seed; the caller passes the ones
	/// a generated query names, so the reference and the engine answer the same question.
	///
	/// # Errors
	///
	/// `metrics` of 0 or more than ten, an empty or unknown host, or an empty window.
	pub fn single_groupby(&self, metrics: usize, hosts: &[&str], start_ns: i64, end_ns: i64) -> anyhow::Result<Vec<MinuteMax>> {
		anyhow::ensure!((1..=CPU_METRICS.len()).contains(&metrics), "a single-groupby takes 1 to {} metrics, not {metrics}", CPU_METRICS.len());
		Ok(self.bucket_max(metrics, hosts, start_ns, end_ns, MINUTE_NS)?.into_iter().map(|(minute_ns, max)| MinuteMax { minute_ns, max }).collect())
	}

	/// `cpu-max-all`'s answer: for each one-hour bucket in `[start_ns, end_ns)` that holds a
	/// reading of one of `hosts`, the MAX of each of the ten [`CPU_METRICS`], in bucket order.
	/// Buckets with no reading are absent.
	///
	/// # Errors
	///
	/// An empty or unknown host, or an empty window.
	pub fn cpu_max_all(&self, hosts: &[&str], start_ns: i64, end_ns: i64) -> anyhow::Result<Vec<HourMax>> {
		Ok(self.bucket_max(CPU_METRICS.len(), hosts, start_ns, end_ns, HOUR_NS)?.into_iter().map(|(hour_ns, max)| HourMax { hour_ns, max }).collect())
	}

	/// `high-cpu`'s answer: every reading of `hosts` in `[start_ns, end_ns)` whose `usage_user` is
	/// above [`HIGH_CPU_ABOVE`], all ten fields, ordered by host name then time. TSBS's SQL has no
	/// `ORDER BY`, so this order is the comparison's, not the engine's.
	///
	/// # Errors
	///
	/// An empty or unknown host, or an empty window.
	pub fn high_cpu(&self, hosts: &[&str], start_ns: i64, end_ns: i64) -> anyhow::Result<Vec<(String, CpuReading)>> {
		anyhow::ensure!(!hosts.is_empty(), "a TSBS query names at least one host");
		anyhow::ensure!(start_ns < end_ns, "the window [{start_ns}, {end_ns}) is empty");
		let mut sorted = hosts.to_vec();
		sorted.sort_unstable();
		let mut rows = Vec::new();
		for host in sorted {
			let readings = self.hosts.get(host).ok_or_else(|| anyhow::anyhow!("no host {host:?} in the TSBS data"))?;
			let first = readings.partition_point(|r| r.ts_ns < start_ns);
			rows.extend(readings[first..].iter().take_while(|r| r.ts_ns < end_ns).filter(|r| r.values[0] > HIGH_CPU_ABOVE).map(|r| (host.to_string(), r.clone())));
		}
		Ok(rows)
	}

	/// `groupby-orderby-limit`'s answer: the MAX of `usage_user` over every host's readings before
	/// `end_ns`, per one-minute bucket, the [`ORDERBY_LIMIT`] latest buckets, newest first.
	#[must_use]
	pub fn groupby_orderby_limit(&self, end_ns: i64) -> Vec<MinuteMax> {
		let hosts: Vec<&str> = self.hosts.keys().map(String::as_str).collect();
		self.bucket_max(1, &hosts, i64::MIN, end_ns, MINUTE_NS).map_or_else(|_| Vec::new(), |buckets| buckets.into_iter().rev().take(ORDERBY_LIMIT).map(|(minute_ns, max)| MinuteMax { minute_ns, max }).collect())
	}

	/// The MAX of each of the first `metrics` fields over `hosts`' readings in `[start_ns, end_ns)`,
	/// per epoch-aligned bucket of `width_ns`, keyed by bucket start.
	fn bucket_max(&self, metrics: usize, hosts: &[&str], start_ns: i64, end_ns: i64, width_ns: i64) -> anyhow::Result<BTreeMap<i64, Vec<i64>>> {
		anyhow::ensure!(!hosts.is_empty(), "a TSBS query names at least one host");
		anyhow::ensure!(start_ns < end_ns, "the window [{start_ns}, {end_ns}) is empty");
		let mut buckets: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
		for host in hosts {
			let readings = self.hosts.get(*host).ok_or_else(|| anyhow::anyhow!("no host {host:?} in the TSBS data"))?;
			let first = readings.partition_point(|r| r.ts_ns < start_ns);
			for reading in readings[first..].iter().take_while(|r| r.ts_ns < end_ns) {
				let bucket_ns = reading.ts_ns.div_euclid(width_ns) * width_ns;
				let max = buckets.entry(bucket_ns).or_insert_with(|| reading.values[..metrics].to_vec());
				for (m, v) in max.iter_mut().zip(&reading.values[..metrics]) {
					*m = (*m).max(*v);
				}
			}
		}
		Ok(buckets)
	}

	/// `lastpoint`'s answer: every host's latest reading, hosts in byte order of their names.
	#[must_use]
	pub fn lastpoint(&self) -> Vec<(&str, &CpuReading)> {
		self.hosts.iter().filter_map(|(host, readings)| readings.last().map(|r| (host.as_str(), r))).collect()
	}

	/// The half-open span `[first, last + 1 ns)` the readings cover across every host, or `None`
	/// when there are none.
	#[must_use]
	pub fn span(&self) -> Option<(i64, i64)> {
		let first = self.hosts.values().filter_map(|r| r.first()).map(|r| r.ts_ns).min()?;
		let last = self.hosts.values().filter_map(|r| r.last()).map(|r| r.ts_ns).max()?;
		Some((first, last.saturating_add(1)))
	}

	/// `count` seeded instances of `query`, drawn the way `tsbs_generate_queries` draws them: the
	/// named hosts are a uniformly random subset of distinct hosts (`getRandomHosts` in
	/// `uses/devops/common.go`), and the window is `hours` long with its start uniform in
	/// `[first, end - window)` at nanosecond granularity (`TimeInterval.RandWindow` in
	/// `internal/utils/time_interval.go`), over the span the readings cover. A `lastpoint` instance
	/// names every host and the whole span; TSBS's `lastpoint` takes no parameters, so its
	/// instances are all the same question.
	///
	/// # Errors
	///
	/// No readings; more hosts asked for than the data holds; or a window not shorter than the span,
	/// which TSBS refuses too.
	pub fn instances(&self, query: TsbsQuery, count: usize, seed: u64) -> anyhow::Result<Vec<QueryInstance>> {
		use rand::{RngExt as _, SeedableRng as _};

		let (first, end) = self.span().ok_or_else(|| anyhow::anyhow!("no readings to draw {} instances from", query.name()))?;
		let names: Vec<&String> = self.hosts.keys().collect();
		let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
		match query {
			TsbsQuery::SingleGroupby { hosts, hours, .. } | TsbsQuery::CpuMaxAll { hosts, hours } | TsbsQuery::HighCpu { hosts, hours } => {
				anyhow::ensure!(hosts <= names.len(), "{} names {hosts} hosts and the data holds {}", query.name(), names.len());
				// `high-cpu-all` names no host filter: every host.
				let hosts = if matches!(query, TsbsQuery::HighCpu { hosts: 0, .. }) { names.len() } else { hosts };
				let window = i64::from(hours).saturating_mul(3_600_000_000_000);
				let upper = end.saturating_sub(window);
				anyhow::ensure!(upper > first, "{} needs a {hours}-hour window and the data spans {} s", query.name(), (end - first) / 1_000_000_000);
				Ok((0..count)
					.map(|_| {
						let picked = rand::seq::index::sample(&mut rng, names.len(), hosts).into_iter().map(|i| names[i].clone()).collect();
						let start_ns = first + rng.random_range(0..upper - first);
						QueryInstance { hosts: picked, start_ns, end_ns: start_ns + window }
					})
					.collect())
			}
			TsbsQuery::GroupbyOrderbyLimit => {
				// The end of a random one-hour window (`MustRandWindow(time.Hour).End()`); the
				// query has no lower bound, so the instance's start is the data's first instant.
				let window = 3_600_000_000_000_i64;
				let upper = end.saturating_sub(window);
				anyhow::ensure!(upper > first, "{} needs a 1-hour window and the data spans {} s", query.name(), (end - first) / 1_000_000_000);
				let hosts: Vec<String> = names.into_iter().cloned().collect();
				Ok((0..count).map(|_| QueryInstance { hosts: hosts.clone(), start_ns: first, end_ns: first + rng.random_range(0..upper - first) + window }).collect())
			}
			TsbsQuery::Lastpoint => Ok(vec![QueryInstance { hosts: names.into_iter().cloned().collect(), start_ns: first, end_ns: end }; count]),
		}
	}

	/// The reference answer to one instance of `query`.
	///
	/// # Errors
	///
	/// As [`Self::single_groupby`].
	pub fn answer(&self, query: TsbsQuery, instance: &QueryInstance) -> anyhow::Result<TsbsAnswer> {
		match query {
			TsbsQuery::SingleGroupby { metrics, .. } => {
				let hosts: Vec<&str> = instance.hosts.iter().map(String::as_str).collect();
				Ok(TsbsAnswer::Minutes(self.single_groupby(metrics, &hosts, instance.start_ns, instance.end_ns)?))
			}
			TsbsQuery::CpuMaxAll { .. } => {
				let hosts: Vec<&str> = instance.hosts.iter().map(String::as_str).collect();
				Ok(TsbsAnswer::Hours(self.cpu_max_all(&hosts, instance.start_ns, instance.end_ns)?))
			}
			TsbsQuery::HighCpu { .. } => {
				let hosts: Vec<&str> = instance.hosts.iter().map(String::as_str).collect();
				Ok(TsbsAnswer::Readings(self.high_cpu(&hosts, instance.start_ns, instance.end_ns)?))
			}
			TsbsQuery::GroupbyOrderbyLimit => Ok(TsbsAnswer::Minutes(self.groupby_orderby_limit(instance.end_ns))),
			TsbsQuery::Lastpoint => Ok(TsbsAnswer::Latest(self.lastpoint().into_iter().map(|(h, r)| (h.to_string(), r.clone())).collect())),
		}
	}
}

/// One generated instance of a [`TsbsQuery`]: the hosts it names and its half-open window, as one
/// line of a `tsbs_generate_queries` file would carry them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInstance {
	/// The `hostname` tags the query names, in the order drawn.
	pub hosts: Vec<String>,
	/// The window's start, in nanoseconds since the Unix epoch (inclusive).
	pub start_ns: i64,
	/// The window's end (exclusive).
	pub end_ns: i64,
}

/// An answer to one [`QueryInstance`], from the reference or from an engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsbsAnswer {
	/// A `single-groupby`: one row per one-minute bucket.
	Minutes(Vec<MinuteMax>),
	/// A `cpu-max-all`: one row per one-hour bucket.
	Hours(Vec<HourMax>),
	/// A `high-cpu`: the matching readings, by host name then time.
	Readings(Vec<(String, CpuReading)>),
	/// A `lastpoint`: one row per host, in name order.
	Latest(Vec<(String, CpuReading)>),
}

impl TsbsAnswer {
	/// How many rows the answer holds.
	#[must_use]
	pub const fn rows(&self) -> usize {
		match self {
			Self::Minutes(rows) => rows.len(),
			Self::Hours(rows) => rows.len(),
			Self::Readings(rows) | Self::Latest(rows) => rows.len(),
		}
	}
}

/// Read every host's `cpu` readings from the TSBS file at `path`, up to `span_secs` after the
/// file's first reading.
///
/// # Errors
///
/// See [`load_cpu_from_reader`].
pub fn load_cpu(path: &Path, span_secs: i64) -> anyhow::Result<TsbsCpu> {
	let file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
	load_cpu_from_reader(std::io::BufReader::new(file), span_secs).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// The reader half of [`load_cpu`]. Stops at the first line past the span, as [`load_from_reader`]
/// does.
///
/// # Errors
///
/// A malformed line; a `cpu` line without a `hostname` tag or a timestamp, or with a
/// [`CPU_METRICS`] field missing or not an `i` integer; a host whose tag set changes; a host's
/// readings that do not strictly increase; or no reading at all.
pub fn load_cpu_from_reader(reader: impl BufRead, span_secs: i64) -> anyhow::Result<TsbsCpu> {
	let mut cpu = TsbsCpu::default();
	let mut keys: BTreeMap<String, SeriesKey> = BTreeMap::new();
	let mut end_ns = None;
	for (idx, line) in reader.lines().enumerate() {
		let line_no = idx + 1;
		let line = line.map_err(|e| anyhow::anyhow!("line {line_no}: {e}"))?;
		let records = weft_line_protocol::parse(&line).map_err(|e| anyhow::anyhow!("line {line_no}: {e}"))?;
		let Some(record) = records.into_iter().next() else { continue };
		if record.measurement != MEASUREMENT {
			continue;
		}
		let ts_ns = record.timestamp.ok_or_else(|| anyhow::anyhow!("line {line_no}: a {MEASUREMENT} line without a timestamp"))?;
		match end_ns {
			Some(end) if ts_ns > end => break,
			Some(_) => {}
			None => end_ns = Some(ts_ns.saturating_add(span_secs.saturating_mul(1_000_000_000))),
		}
		let host = record.tag("hostname").ok_or_else(|| anyhow::anyhow!("line {line_no}: a {MEASUREMENT} line without a hostname tag"))?.to_string();
		let mut values = [0_i64; 10];
		for (slot, metric) in values.iter_mut().zip(CPU_METRICS) {
			*slot = match record.field(metric) {
				Some(FieldValue::Integer(v)) => *v,
				Some(other) => anyhow::bail!("line {line_no}: {host}'s {metric} is {other:?}, not an integer"),
				None => anyhow::bail!("line {line_no}: {host} has no {metric}"),
			};
		}
		let key = record.series_key();
		match keys.get(&host) {
			None => {
				keys.insert(host.clone(), key);
			}
			Some(first) if *first != key => anyhow::bail!("line {line_no}: {host} changed its tag set to {}", record.describe_series()),
			Some(_) => {}
		}
		let readings = cpu.hosts.entry(host).or_default();
		if let Some(last) = readings.last() {
			anyhow::ensure!(ts_ns > last.ts_ns, "line {line_no}: a reading at {ts_ns} does not follow {}", last.ts_ns);
		}
		readings.push(CpuReading { ts_ns, values });
	}
	anyhow::ensure!(!cpu.is_empty(), "no {MEASUREMENT} readings");
	Ok(cpu)
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
	/// Three hosts with all ten fields, a reading every 30 s from 2016-01-01T00:00:00Z, so two
	/// readings per minute; `host_10` sorts before `host_2`.
	fn cpu_sample() -> String {
		use std::fmt::Write as _;

		let mut out = String::new();
		for step in 0..6_i64 {
			for (h, host) in ["host_0", "host_2", "host_10"].iter().enumerate() {
				let h = i64::try_from(h).expect("small");
				let fields: Vec<String> = CPU_METRICS.iter().enumerate().map(|(m, name)| format!("{name}={}i", 10 * h + step + i64::try_from(m).expect("small") * 100)).collect();
				writeln!(out, "cpu,hostname={host},region=r{h} {} {}", fields.join(","), 1_451_606_400_000_000_000 + step * 30_000_000_000).expect("writes to a String");
			}
		}
		out
	}

	#[test]
	fn reads_every_hosts_ten_fields() {
		let cpu = load_cpu_from_reader(cpu_sample().as_bytes(), 3600).expect("reads");
		assert_eq!(cpu.hosts.keys().map(String::as_str).collect::<Vec<_>>(), ["host_0", "host_10", "host_2"]);
		assert_eq!(cpu.len(), 18);
		let first = &cpu.hosts["host_2"][0];
		assert_eq!((first.ts_ns, first.values[0], first.values[9]), (1_451_606_400_000_000_000, 10, 910));
		assert_eq!(load_cpu_from_reader(cpu_sample().as_bytes(), 30).expect("reads").len(), 6, "the span keeps the first two instants");
	}

	#[test]
	fn single_groupby_is_the_per_minute_max_over_the_named_hosts() {
		let cpu = load_cpu_from_reader(cpu_sample().as_bytes(), 3600).expect("reads");
		let t0 = 1_451_606_400_000_000_000_i64;
		// Steps 0..6 at 30 s are minutes 0, 0, 1, 1, 2, 2; host_2's usage_user is 10 + step.
		let one = cpu.single_groupby(1, &["host_2"], t0, t0 + 3 * MINUTE_NS).expect("answers");
		assert_eq!(one, [MinuteMax { minute_ns: t0, max: vec![11] }, MinuteMax { minute_ns: t0 + MINUTE_NS, max: vec![13] }, MinuteMax { minute_ns: t0 + 2 * MINUTE_NS, max: vec![15] }]);
		// Two hosts and two metrics: host_10 (20 + step) dominates host_0 (step) in every bucket.
		let two = cpu.single_groupby(2, &["host_0", "host_10"], t0, t0 + 2 * MINUTE_NS).expect("answers");
		assert_eq!(two.iter().map(|r| r.max.clone()).collect::<Vec<_>>(), [vec![21, 121], vec![23, 123]]);
		// The window is half-open and cuts inside a bucket: only step 1 (t0 + 30 s) is in it.
		let cut = cpu.single_groupby(1, &["host_0"], t0 + 1, t0 + MINUTE_NS).expect("answers");
		assert_eq!(cut, [MinuteMax { minute_ns: t0, max: vec![1] }]);
		assert!(cpu.single_groupby(1, &["host_0"], t0 + 10 * MINUTE_NS, t0 + 11 * MINUTE_NS).expect("answers").is_empty(), "no reading, no bucket");
	}

	#[test]
	fn cpu_max_all_is_the_per_hour_max_of_every_metric_over_the_named_hosts() {
		let t0 = 1_451_606_400_000_000_000_i64;
		let mut cpu = TsbsCpu::default();
		// Two hosts, a reading every twenty minutes for three hours; metric m of host h at step s
		// is 100 * h + 10 * m + s, so host_1 dominates and the last step in each hour is the MAX.
		for h in 0..2_i64 {
			let readings = (0..9_i64).map(|s| CpuReading { ts_ns: t0 + s * 20 * MINUTE_NS, values: std::array::from_fn(|m| 100 * h + 10 * i64::try_from(m).expect("small") + s) }).collect();
			cpu.hosts.insert(format!("host_{h}"), readings);
		}
		let both = cpu.cpu_max_all(&["host_0", "host_1"], t0, t0 + 3 * HOUR_NS).expect("answers");
		assert_eq!(both.iter().map(|r| r.hour_ns).collect::<Vec<_>>(), [t0, t0 + HOUR_NS, t0 + 2 * HOUR_NS]);
		assert_eq!(both[0].max, (0..10).map(|m| 100 + 10 * m + 2).collect::<Vec<_>>());
		assert_eq!(both[2].max.len(), 10, "all ten metrics");
		// Half-open and cut inside an hour: steps 1 and 2 of the first hour, then step 3 alone.
		let cut = cpu.cpu_max_all(&["host_0"], t0 + 1, t0 + HOUR_NS + 1).expect("answers");
		assert_eq!(cut.iter().map(|r| (r.hour_ns, r.max[0])).collect::<Vec<_>>(), [(t0, 2), (t0 + HOUR_NS, 3)]);
		assert!(cpu.cpu_max_all(&["host_9"], t0, t0 + HOUR_NS).is_err(), "an unknown host is refused");
		let drawn = cpu.instances(TsbsQuery::CpuMaxAll { hosts: 2, hours: 1 }, 2, 7).expect("draws");
		assert!(drawn.iter().all(|i| i.hosts.len() == 2 && i.end_ns - i.start_ns == HOUR_NS));
		assert!(cpu.instances(TsbsQuery::CpuMaxAll { hosts: 1, hours: MAX_ALL_HOURS }, 1, 7).expect_err("too short").to_string().contains("8-hour window"));
	}

	#[test]
	fn high_cpu_is_every_reading_above_ninety_by_host_then_time() {
		let t0 = 1_451_606_400_000_000_000_i64;
		let mut cpu = TsbsCpu::default();
		// usage_user of host h at step s is 85 + 3 * s + h, so step 2 is the first above 90 on both.
		for h in 0..2_i64 {
			let readings = (0..5_i64).map(|s| CpuReading { ts_ns: t0 + s * MINUTE_NS, values: std::array::from_fn(|m| if m == 0 { 85 + 3 * s + h } else { i64::try_from(m).expect("small") }) }).collect();
			cpu.hosts.insert(format!("host_{h}"), readings);
		}
		let got = |hosts: &[&str], start: i64, end: i64| cpu.high_cpu(hosts, start, end).expect("answers").into_iter().map(|(h, r)| (h, (r.ts_ns - t0) / MINUTE_NS, r.values[0])).collect::<Vec<_>>();
		let host = |h: &str| h.to_string();
		// Hosts in name order whatever order they are named in; 88 and 89 are not above 90.
		assert_eq!(got(&["host_1", "host_0"], t0, t0 + 5 * MINUTE_NS), [(host("host_0"), 2, 91), (host("host_0"), 3, 94), (host("host_0"), 4, 97), (host("host_1"), 2, 92), (host("host_1"), 3, 95), (host("host_1"), 4, 98)]);
		// Half-open: the reading at the end instant is out.
		assert_eq!(got(&["host_0"], t0, t0 + 3 * MINUTE_NS), [(host("host_0"), 2, 91)]);
		assert!(cpu.high_cpu(&["host_0"], t0, t0 + 2 * MINUTE_NS).expect("answers").is_empty(), "nothing above 90, no row");
		assert_eq!(cpu.high_cpu(&["host_0"], t0, t0 + 5 * MINUTE_NS).expect("answers")[0].1.values[9], 9, "all ten fields");
		assert!(cpu.high_cpu(&["host_9"], t0, t0 + MINUTE_NS).is_err());
	}

	#[test]
	fn groupby_orderby_limit_is_the_five_latest_minutes_before_the_end_newest_first() {
		let t0 = 1_451_606_400_000_000_000_i64;
		let mut cpu = TsbsCpu::default();
		// host_0 reads every 30 s for ten minutes, usage_user = step; host_1 reads once a minute,
		// usage_user = 100 in minutes 3 and 7 only, so it wins those buckets.
		cpu.hosts.insert("host_0".into(), (0..20_i64).map(|s| CpuReading { ts_ns: t0 + s * 30_000_000_000, values: std::array::from_fn(|m| if m == 0 { s } else { 0 }) }).collect());
		cpu.hosts.insert("host_1".into(), (0..10_i64).map(|s| CpuReading { ts_ns: t0 + s * MINUTE_NS, values: std::array::from_fn(|m| if m == 0 && (s == 3 || s == 7) { 100 } else { 0 }) }).collect());
		let got = |end: i64| cpu.groupby_orderby_limit(end).into_iter().map(|r| ((r.minute_ns - t0) / MINUTE_NS, r.max[0])).collect::<Vec<_>>();
		assert_eq!(got(t0 + 10 * MINUTE_NS), [(9, 19), (8, 17), (7, 100), (6, 13), (5, 11)]);
		// The end is exclusive and may cut a minute: before t0 + 8 min + 30 s, minute 8 holds step 16 only.
		assert_eq!(got(t0 + 8 * MINUTE_NS + 30_000_000_000), [(8, 16), (7, 100), (6, 13), (5, 11), (4, 9)]);
		// Fewer than five minutes before the end: all of them.
		assert_eq!(got(t0 + 2 * MINUTE_NS), [(1, 3), (0, 1)]);
		assert_eq!(got(t0), Vec::<(i64, i64)>::new(), "nothing before the first reading");
	}

	#[test]
	fn single_groupby_refuses_a_question_it_cannot_answer() {
		let cpu = load_cpu_from_reader(cpu_sample().as_bytes(), 3600).expect("reads");
		assert!(cpu.single_groupby(0, &["host_0"], 0, 1).is_err());
		assert!(cpu.single_groupby(11, &["host_0"], 0, 1).is_err());
		assert!(cpu.single_groupby(1, &[], 0, 1).is_err());
		assert!(cpu.single_groupby(1, &["host_0"], 1, 1).is_err());
		assert!(cpu.single_groupby(1, &["host_9"], 0, 1).expect_err("unknown host").to_string().contains("host_9"));
	}

	#[test]
	fn lastpoint_is_each_hosts_latest_reading_in_name_order() {
		let cpu = load_cpu_from_reader(cpu_sample().as_bytes(), 3600).expect("reads");
		let last: Vec<(&str, i64, i64)> = cpu.lastpoint().into_iter().map(|(h, r)| (h, r.ts_ns, r.values[0])).collect();
		let t5 = 1_451_606_400_000_000_000 + 5 * 30_000_000_000;
		assert_eq!(last, [("host_0", t5, 5), ("host_10", t5, 25), ("host_2", t5, 15)]);
	}

	#[test]
	fn instances_draw_distinct_hosts_and_a_window_inside_the_span_as_tsbs_does() {
		use std::fmt::Write as _;

		let cpu = load_cpu_from_reader(cpu_sample().as_bytes(), 3600).expect("reads");
		let (first, end) = cpu.span().expect("has readings");
		assert_eq!((first, end), (1_451_606_400_000_000_000, 1_451_606_400_000_000_000 + 150_000_000_001));
		// The sample spans 150 s, so a one-hour window does not fit: TSBS refuses that too.
		assert!(cpu.instances(TsbsQuery::SingleGroupby { metrics: 1, hosts: 1, hours: 1 }, 3, 7).expect_err("too short").to_string().contains("1-hour window"));
		let mut long = String::new();
		for step in 0..200_i64 {
			for host in ["host_0", "host_1", "host_2", "host_3"] {
				let fields: Vec<String> = CPU_METRICS.iter().map(|m| format!("{m}={step}i")).collect();
				writeln!(long, "cpu,hostname={host} {} {}", fields.join(","), 1_451_606_400_000_000_000 + step * 60_000_000_000).expect("writes to a String");
			}
		}
		let cpu = load_cpu_from_reader(long.as_bytes(), 86_400).expect("reads");
		let (first, end) = cpu.span().expect("has readings");
		let query = TsbsQuery::SingleGroupby { metrics: 5, hosts: 3, hours: 1 };
		let drawn = cpu.instances(query, 50, 7).expect("draws");
		assert_eq!(drawn, cpu.instances(query, 50, 7).expect("draws"), "seeded");
		assert_ne!(drawn, cpu.instances(query, 50, 8).expect("draws"), "another seed, other instances");
		for i in &drawn {
			assert_eq!(i.end_ns - i.start_ns, 3_600_000_000_000);
			assert!(i.start_ns >= first && i.end_ns <= end, "{i:?} inside [{first}, {end})");
			let distinct: BTreeSet<&String> = i.hosts.iter().collect();
			assert_eq!((i.hosts.len(), distinct.len()), (3, 3), "{i:?}");
			assert!(i.hosts.iter().all(|h| cpu.hosts.contains_key(h)));
		}
		assert!(drawn.iter().map(|i| i.start_ns).collect::<BTreeSet<_>>().len() > 40, "starts are drawn, not fixed");
		assert!(cpu.instances(TsbsQuery::SingleGroupby { metrics: 1, hosts: 5, hours: 1 }, 1, 7).expect_err("five of four").to_string().contains("holds 4"));
		let last = cpu.instances(TsbsQuery::Lastpoint, 2, 7).expect("draws");
		assert_eq!(last[0], last[1]);
		assert_eq!(last[0].hosts, ["host_0", "host_1", "host_2", "host_3"]);
		let TsbsAnswer::Latest(rows) = cpu.answer(TsbsQuery::Lastpoint, &last[0]).expect("answers") else { panic!("a lastpoint answer") };
		assert_eq!(rows.iter().map(|(_, r)| r.values[0]).collect::<Vec<_>>(), [199, 199, 199, 199]);
		let answer = cpu.answer(query, &drawn[0]).expect("answers");
		assert_eq!(answer.rows(), 60, "a one-hour window over one reading a minute is sixty buckets");
	}

	#[test]
	fn query_names_round_trip_as_tsbs_spells_them() {
		for q in TsbsQuery::ALL {
			assert_eq!(TsbsQuery::parse(&q.name()).expect("parses"), q);
		}
		assert_eq!(TsbsQuery::ALL[0].name(), "single-groupby-1-1-1");
		assert_eq!(TsbsQuery::ALL[5].name(), "single-groupby-5-8-1");
		assert_eq!((TsbsQuery::ALL[6].name().as_str(), TsbsQuery::ALL[7].name().as_str()), ("cpu-max-all-1", "cpu-max-all-8"));
		assert_eq!(TsbsQuery::CpuMaxAll { hosts: 32, hours: 24 }.name(), "cpu-max-all-32-24");
		assert!(TsbsQuery::parse("cpu-max-all-32-24").is_err(), "not in ALL: the reference file holds ten hosts");
		assert_eq!((TsbsQuery::ALL[8].name().as_str(), TsbsQuery::ALL[9].name().as_str()), ("high-cpu-1", "high-cpu-all"));
		assert_eq!(TsbsQuery::ALL[10].name(), "groupby-orderby-limit");
		assert!(TsbsQuery::parse("double-groupby-1").expect_err("not yet").to_string().contains("supported: single-groupby-1-1-1"));
	}

	#[test]
	fn load_cpu_refuses_what_tsbs_does_not_write() {
		let ok = |fields: &str| format!("cpu,hostname=host_0 {fields} 10\n");
		let all: Vec<String> = CPU_METRICS.iter().map(|m| format!("{m}=1i")).collect();
		assert!(load_cpu_from_reader(ok(&all.join(",")).as_bytes(), 3600).is_ok());
		let float = all.join(",").replacen("usage_user=1i", "usage_user=1.5", 1);
		assert!(load_cpu_from_reader(ok(&float).as_bytes(), 3600).expect_err("float").to_string().contains("not an integer"));
		assert!(load_cpu_from_reader(ok(&all[1..].join(",")).as_bytes(), 3600).expect_err("missing").to_string().contains("has no usage_user"));
		let untagged = format!("cpu {} 10\n", all.join(","));
		assert!(load_cpu_from_reader(untagged.as_bytes(), 3600).expect_err("no host").to_string().contains("hostname"));
		let backwards = format!("cpu,hostname=host_0 {0} 20\ncpu,hostname=host_0 {0} 10\n", all.join(","));
		assert!(load_cpu_from_reader(backwards.as_bytes(), 3600).expect_err("backwards").to_string().contains("does not follow"));
		let retagged = format!("cpu,hostname=host_0,rack=1 {0} 10\ncpu,hostname=host_0,rack=2 {0} 20\n", all.join(","));
		assert!(load_cpu_from_reader(retagged.as_bytes(), 3600).expect_err("two tag sets").to_string().contains("changed its tag set"));
		assert!(load_cpu_from_reader(&b"mem,hostname=host_0 used=1i 10\n"[..], 3600).expect_err("empty").to_string().contains("no cpu readings"));
	}
}
