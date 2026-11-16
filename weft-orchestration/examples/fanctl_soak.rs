//! Reproduces the weftdb workload of the Omarchy fan daemon (fanctl) to find what grows
//! its memory and burns its CPU. It makes the same weftdb calls fanctl makes, at the same
//! cadence in *virtual* time:
//!
//! - every second: one `capture_measurement` per temperature sensor, one per fan duty and
//!   rpm, one for the controller duty, and one `analyze_point` (polynomial trend) per
//!   sensor;
//! - every minute: one rollup capture per sensor into the `temperature_1m` aspects;
//! - every 30 s: the spike forecast loop (`get_events` + `query_probability`), and every
//!   10 min a full `Pipeline::run` per sensor (threshold detector + a 10-step dictionary),
//!   once the sensor has 45 min of rollups;
//! - every hour: `compress` on every raw aspect and a 28-day `analyze_range` profile
//!   rebuild per sensor.
//!
//! Virtual time starts `--hours` before the real clock and runs as fast as the machine
//! allows (or in real time with `--realtime`), so a 3 h soak takes minutes. Each
//! component can be turned off to bisect.
//!
//! ```text
//! cargo run --release -p weft-orchestration --example fanctl_soak -- <empty dir> \
//!     [--hours 3] [--sensors 22] [--fans 4] [--sample-min 10] [--realtime] \
//!     [--no-spikes] [--no-predict] [--no-profile] [--no-compress]
//! ```
//!
//! It prints one CSV row per sample: virtual minutes, wall seconds, process CPU seconds,
//! anonymous memory (from `/proc/self/smaps_rollup`), RSS, read syscalls and bytes (from
//! `/proc/self/io`), files and bytes under the data dir, and the size of every `-log`.
//! Set `WEFT_MVCC_CHECKPOINT_BYTES` to A/B the checkpoint threshold.
//!
//! Caveat: weftdb's own wall-clock timers (idle close, queue pruning) still run in real
//! time, so a compressed run sees fewer of them per virtual hour than fanctl does.

use std::{
	collections::HashMap, path::{Path, PathBuf}, str::FromStr, sync::Arc, time::{Duration as StdDuration, Instant}
};

use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, ToPrimitive};
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use weft_orchestration::{
	detect_threshold_crossing_up_in_window, pipeline::{EventDetector, EventDetectorFn}, Pipeline
};
use weftdb::{
	database::traits::{AspectStructure, Inputs}, AspectId, CompressionConfig, Database, DatabaseStructure, DatasetId, DictionaryConstraints, InputMeasurement, Outputs, Resolution, SignalType, SizeBasedCompressionConfig, Spline, Steps, SubjectId, TimeBasedCompressionConfig
};

static T: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
const T_NAMES: [&str; 6] = ["capture", "predict", "fc_query", "fc_run", "hourly", "fc_build"];
fn add(i: usize, t: Instant) {
	T[i].fetch_add(t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}
fn take_times() -> String {
	T.iter().zip(T_NAMES).map(|(a, n)| format!("{n}={:.1}s", a.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / 1e6)).collect::<Vec<_>>().join(" ")
}

const DB_NAME: &str = "fanctl-soak";
const RUN_EVERY: i64 = 600;
const QUERY_EVERY: i64 = 30;
const MIN_HISTORY: Duration = Duration::minutes(45);
const HORIZON: Duration = Duration::seconds(20);
const THRESHOLD: f64 = 70.0;

struct Opts {
	dir: PathBuf,
	hours: f64,
	sensors: usize,
	fans: usize,
	sample_min: i64,
	realtime: bool,
	spikes: bool,
	predict: bool,
	profile: bool,
	compress: bool,
}

fn parse() -> Result<Opts> {
	let mut a = std::env::args().skip(1);
	let dir = PathBuf::from(a.next().context("usage: fanctl_soak <empty dir> [options]")?);
	let mut o = Opts { dir, hours: 3.0, sensors: 22, fans: 4, sample_min: 10, realtime: false, spikes: true, predict: true, profile: true, compress: true };
	while let Some(flag) = a.next() {
		let mut val = || a.next().context("missing value");
		match flag.as_str() {
			"--hours" => o.hours = val()?.parse()?,
			"--sensors" => o.sensors = val()?.parse()?,
			"--fans" => o.fans = val()?.parse()?,
			"--sample-min" => o.sample_min = val()?.parse()?,
			"--realtime" => o.realtime = true,
			"--no-spikes" => o.spikes = false,
			"--no-predict" => o.predict = false,
			"--no-profile" => o.profile = false,
			"--no-compress" => o.compress = false,
			other => anyhow::bail!("unknown flag {other}"),
		}
	}
	Ok(o)
}

// ---- process and data-dir probes -------------------------------------------------------

fn proc_kb(file: &str, key: &str) -> u64 {
	std::fs::read_to_string(file).ok().and_then(|s| s.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}

fn cpu_secs() -> f64 {
	// utime + stime, fields 14 and 15 of /proc/self/stat, in clock ticks (100 Hz on Linux).
	let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
	let rest = s.rsplit_once(')').map_or("", |(_, r)| r);
	let f: Vec<&str> = rest.split_whitespace().collect();
	let t = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
	(t(11) + t(12)) / 100.0
}

fn dir_stats(dir: &Path) -> (u64, u64, u64) {
	let (mut files, mut bytes, mut logs) = (0, 0, 0);
	let mut stack = vec![dir.to_path_buf()];
	while let Some(d) = stack.pop() {
		for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
			let Ok(m) = e.metadata() else { continue };
			if m.is_dir() {
				stack.push(e.path());
			} else {
				files += 1;
				bytes += m.len();
				if e.file_name().to_string_lossy().ends_with("-log") {
					logs += m.len();
				}
			}
		}
	}
	(files, bytes, logs)
}

// ---- fanctl's store.rs, minus hwmon --------------------------------------------------

fn compression() -> CompressionConfig {
	let time = TimeBasedCompressionConfig::new(Duration::days(1), Duration::days(1)).with_max_tiers(7);
	let size = SizeBasedCompressionConfig::new(256 * 1024 * 1024);
	CompressionConfig::combined(time, size).with_base_resolution(Resolution::Minutes)
}

struct Store {
	db: Database,
	dataset: DatasetId,
	subjects: HashMap<&'static str, SubjectId>,
	aspects: HashMap<String, AspectId>,
}

impl Store {
	async fn open() -> Result<Self> {
		let db = match Database::existing(DB_NAME).await {
			Ok(db) => db,
			Err(_) => Database::new(DB_NAME).await.context("creating weftdb database")?,
		};
		Ok(Self { db, dataset: DatasetId::new(), subjects: HashMap::new(), aspects: HashMap::new() })
	}

	async fn subject(&mut self, name: &'static str) -> Result<SubjectId> {
		if let Some(id) = self.subjects.get(name) {
			return Ok(*id);
		}
		let s = match self.db.get_subject_by_name(name).await {
			Ok(s) => s,
			Err(_) => self.db.observe_subject(name).await?,
		};
		self.subjects.insert(name, s.id());
		Ok(s.id())
	}

	async fn aspect(&mut self, subject: &'static str, name: &str) -> Result<AspectId> {
		let key = format!("{subject}/{name}");
		if let Some(id) = self.aspects.get(&key) {
			return Ok(*id);
		}
		let sid = self.subject(subject).await?;
		let existing = self.db.list_aspects(&sid).await?;
		let aspect = match existing.into_iter().find(|a| a.name() == name) {
			Some(a) => a,
			None if subject == "temperature_1m" => self.db.track_aspect(&sid, name, &Resolution::Minutes, None).await?,
			None => self.db.track_aspect(&sid, name, &Resolution::Seconds, Some(compression())).await?,
		};
		self.aspects.insert(key, aspect.id());
		Ok(aspect.id())
	}

	async fn capture(&mut self, subject: &'static str, name: &str, at: DateTime<Utc>, value: f64) -> Result<()> {
		let t0 = Instant::now();
		let id = self.aspect(subject, name).await?;
		let v = BigDecimal::from_str(&format!("{value:.3}"))?;
		let r = self.db.capture_measurement(&id, &self.dataset, &InputMeasurement::new(at, v)).await;
		add(0, t0);
		r?;
		Ok(())
	}

	async fn predict(&mut self, name: &str, now: DateTime<Utc>) -> Result<f64> {
		let id = self.aspect("temperature", name).await?;
		let p = self.db.analyze_point(&id, now + HORIZON, &Resolution::Seconds, &Spline::Polynomial(2, Some(1.25))).await?;
		p.value.to_f64().context("prediction out of f64 range")
	}

	async fn compress_all(&mut self) {
		for id in self.aspects.values() {
			if let Ok(mut a) = self.db.get_aspect(id).await {
				if a.compression_config().is_some() {
					if let Err(e) = a.compress(&self.db).await {
						eprintln!("compress {}: {e:#}", a.name());
					}
				}
			}
		}
	}
}

// ---- fanctl's forecast.rs and profile.rs, driven by the virtual clock ---------------------

fn threshold_detector(threshold: f64) -> EventDetector {
	let t = BigDecimal::from_str(&format!("{threshold:.1}")).unwrap_or_default();
	let since: Arc<std::sync::Mutex<Option<DateTime<Utc>>>> = Arc::default();
	let f: EventDetectorFn = Arc::new(move |db, aspect, res, method| {
		let t = t.clone();
		let since = since.clone();
		Box::pin(async move {
			let from = *since.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			let latest = weftdb::DatabaseStructure::get_latest_measurement(db, aspect).await.ok().flatten();
			let events = detect_threshold_crossing_up_in_window(db, aspect, res, method, t, "spike", from).await?;
			if latest.is_some() {
				*since.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = latest;
			}
			Ok(events)
		})
	});
	EventDetector::new(format!("spike_{threshold:.0}c"), format!("Spike above {threshold:.0} °C"), None, f)
}

struct Forecast {
	pipelines: HashMap<String, Pipeline>,
	last_run: HashMap<String, DateTime<Utc>>,
	last_query: Option<DateTime<Utc>>,
	runs: u64,
	errors: u64,
}

impl Forecast {
	async fn tick(&mut self, db: &Database, jobs: &[(String, AspectId)], now: DateTime<Utc>) {
		if self.last_query.is_some_and(|q| (now - q).num_seconds() < QUERY_EVERY) {
			return;
		}
		self.last_query = Some(now);
		for (key, aspect) in jobs {
			if !self.pipelines.contains_key(key) {
				let t0 = Instant::now();
				let _guard = scopeguard(move || add(5, t0));
				let earliest = db.get_earliest_measurement(aspect).await.ok().flatten();
				if earliest.is_none_or(|e| now - e < MIN_HISTORY) {
					continue;
				}
				let constraints = DictionaryConstraints::new(Some(Steps::new(10, Spline::Cubic)), None);
				match Pipeline::builder(db.clone(), *aspect).spline_method(Spline::Linear).batch_size(30).add_dictionary("preamble", "Shapes that precede a temperature spike", constraints).with_detector(threshold_detector(THRESHOLD)).build().await {
					Ok(p) => {
						self.pipelines.insert(key.clone(), p);
					}
					Err(e) => {
						self.errors += 1;
						eprintln!("pipeline build {key}: {e:#}");
						continue;
					}
				}
			}
			let Some(p) = self.pipelines.get_mut(key) else { continue };
			if self.last_run.get(key).is_none_or(|t| (now - *t).num_seconds() >= RUN_EVERY) {
				self.last_run.insert(key.clone(), now);
				self.runs += 1;
				let t0 = Instant::now();
				let res = run_phased(p).await;
				add(3, t0);
				match res {
					Ok(ms) => eprintln!("phases {key} at {now}: prepare={} patterns={} detect={} correlate={} signals={} save={} ms", ms[0], ms[1], ms[2], ms[3], ms[4], ms[5]),
					Err(e) => {
						self.errors += 1;
						eprintln!("pipeline run {key}: {e:#}");
					}
				}
			}
			let t0 = Instant::now();
			let events = p.get_events().await.unwrap_or_default();
			if self.last_run.get(key) == Some(&now) {
				let q0 = Instant::now();
				let mut n = 0;
				for ev in &events {
					if p.query_probability(ev.id(), &SignalType::PredictStart, now + HORIZON).await.is_ok() {
						n += 1;
					}
				}
				let corr = Outputs::get_correlations(db, aspect).await.map(|st| st.count());
				let corr = match corr {
					Ok(f) => f.await,
					Err(_) => 0,
				};
				eprintln!("run {key} at {now}: correlations={corr} events={} signals={} probability_queries_ok={n} query_ms={}", events.len(), p.signals().len(), q0.elapsed().as_millis());
				add(2, t0);
				continue;
			}
			for ev in &events {
				let _ = p.query_probability(ev.id(), &SignalType::PredictStart, now + HORIZON).await;
			}
			add(2, t0);
		}
	}
}

/// `Pipeline::run`, one step at a time, timing each (`run` adds only a logged run count).
struct Guard<F: FnMut()>(F);
impl<F: FnMut()> Drop for Guard<F> {
	fn drop(&mut self) {
		(self.0)();
	}
}
fn scopeguard<F: FnMut()>(f: F) -> Guard<F> {
	Guard(f)
}

async fn run_phased(p: &mut Pipeline) -> Result<[u128; 6]> {
	let mut ms = [0u128; 6];
	let t = Instant::now();
	p.prepare_data().await?;
	ms[0] = t.elapsed().as_millis();
	let t = Instant::now();
	p.extract_patterns().await?;
	ms[1] = t.elapsed().as_millis();
	let t = Instant::now();
	p.detect_events().await?;
	ms[2] = t.elapsed().as_millis();
	let t = Instant::now();
	p.correlate_events().await?;
	ms[3] = t.elapsed().as_millis();
	let t = Instant::now();
	p.generate_signals().await?;
	ms[4] = t.elapsed().as_millis();
	let t = Instant::now();
	p.save().await?;
	ms[5] = t.elapsed().as_millis();
	Ok(ms)
}

async fn rebuild_profiles(db: &Database, jobs: &[(String, AspectId)], now: DateTime<Utc>) {
	for (_, aspect) in jobs {
		let Ok(Some(first)) = db.get_earliest_measurement(aspect).await else { continue };
		let start = first.max(now - Duration::days(28));
		if now - start < Duration::minutes(60) {
			continue;
		}
		if let Ok(mut s) = db.analyze_range(aspect, start, now, Resolution::Minutes, Spline::Linear).await {
			while let Some(p) = s.next().await {
				if p.is_err() {
					break;
				}
			}
		}
	}
}

/// A plausible CPU temperature: a slow daily-ish wave, noise, and a load spike above
/// `THRESHOLD` for a few minutes every ~25 min (offset per sensor), so the threshold
/// detector finds events and the dictionary sees varied shapes.
fn temp(sensor: usize, t: i64) -> f64 {
	let x = t as f64;
	let phase = sensor as f64 * 97.0;
	let base = 10.0f64.mul_add(((x + phase) / 3600.0).sin(), 48.0);
	let cycle = (t + sensor as i64 * 211) % 1500;
	let spike = if cycle < 240 { 28.0 * (cycle as f64 / 240.0 * std::f64::consts::PI).sin() } else { 0.0 };
	let noise = ((x * 12.9898 + phase).sin() * 43758.5453).fract();
	(base + spike + noise).round()
}

#[tokio::main]
async fn main() -> Result<()> {
	let o = parse()?;
	std::fs::create_dir_all(&o.dir)?;
	anyhow::ensure!(std::fs::read_dir(&o.dir)?.next().is_none(), "{} is not empty", o.dir.display());
	std::env::set_var("WEFT_DATA_DIR", &o.dir);

	let total = Duration::seconds((o.hours * 3600.0) as i64);
	let start = Utc::now() - total;
	let sensors: Vec<String> = (0..o.sensors).map(|i| format!("sensor_{i}")).collect();
	let fans: Vec<String> = (0..o.fans).map(|i| format!("fan_{i}")).collect();

	let mut store = Store::open().await?;
	let mut jobs = Vec::new();
	for s in &sensors {
		jobs.push((s.clone(), store.aspect("temperature_1m", s).await?));
	}
	let mut forecast = Forecast { pipelines: HashMap::new(), last_run: HashMap::new(), last_query: None, runs: 0, errors: 0 };
	let mut minute: HashMap<usize, (f64, u32)> = HashMap::new();
	let mut minute_of = start.timestamp() / 60;
	let mut last_hourly = start;
	let wall0 = Instant::now();
	let cpu0 = cpu_secs();

	eprintln!("fanctl_soak: {} sensors, {} fans, {:.1} virtual h, spikes={} predict={} profile={} compress={} WEFT_MVCC_CHECKPOINT_BYTES={}", o.sensors, o.fans, o.hours, o.spikes, o.predict, o.profile, o.compress, std::env::var("WEFT_MVCC_CHECKPOINT_BYTES").unwrap_or_else(|_| "default".into()));
	println!("vmin,wall_s,cpu_s,anon_mb,rss_mb,syscr,read_mb,files,dir_mb,log_mb,pipeline_runs,errors");
	let sample = |vmin: i64, runs: u64, errors: u64| {
		let (files, bytes, logs) = dir_stats(&o.dir);
		println!("{vmin},{:.0},{:.1},{:.0},{:.0},{},{:.0},{files},{:.1},{:.2},{runs},{errors}", wall0.elapsed().as_secs_f64(), cpu_secs() - cpu0, proc_kb("/proc/self/smaps_rollup", "Anonymous:") as f64 / 1024.0, proc_kb("/proc/self/status", "VmRSS:") as f64 / 1024.0, proc_kb("/proc/self/io", "syscr:"), proc_kb("/proc/self/io", "rchar:") as f64 / 1e6, bytes as f64 / 1e6, logs as f64 / 1e6,);
	};

	let mut tick_at = Instant::now();
	for t in 0..total.num_seconds() {
		let now = start + Duration::seconds(t);
		for (i, s) in sensors.iter().enumerate() {
			let v = temp(i, now.timestamp());
			if let Err(e) = store.capture("temperature", s, now, v).await {
				eprintln!("capture {s}: {e:#}");
			}
			let m = minute.entry(i).or_insert((0.0, 0));
			m.0 += v;
			m.1 += 1;
			if o.predict && t >= 30 {
				let t0 = Instant::now();
				let _ = store.predict(s, now).await;
				add(1, t0);
			}
		}
		if o.spikes {
			forecast.tick(&store.db, &jobs, now).await;
		}
		for (i, f) in fans.iter().enumerate() {
			let duty = 40.0 + (now.timestamp() % 60 + i as i64) as f64;
			let _ = store.capture("fan_duty", f, now, duty).await;
			let _ = store.capture("fan_rpm", f, now, duty * 20.0).await;
		}
		if now.timestamp() / 60 != minute_of {
			let at = DateTime::from_timestamp(minute_of * 60 + 30, 0).unwrap_or(now);
			minute_of = now.timestamp() / 60;
			for (i, (sum, n)) in minute.drain() {
				if n > 0 {
					let _ = store.capture("temperature_1m", &sensors[i], at, sum / f64::from(n)).await;
				}
			}
		}
		if (now - last_hourly).num_seconds() >= 3600 {
			last_hourly = now;
			let t0 = Instant::now();
			let _g = scopeguard(move || add(4, t0));
			if o.compress {
				store.compress_all().await;
			}
			if o.profile {
				rebuild_profiles(&store.db, &jobs, now).await;
			}
		}
		let _ = store.capture("controller", "duty", now, 50.0).await;
		if t % (o.sample_min * 60) == 0 {
			sample(t / 60, forecast.runs, forecast.errors);
			eprintln!("times vmin={}: {}", t / 60, take_times());
		}
		if o.realtime {
			tick_at += StdDuration::from_secs(1);
			tokio::time::sleep_until(tick_at.into()).await;
		}
	}
	sample(total.num_minutes(), forecast.runs, forecast.errors);
	Ok(())
}
