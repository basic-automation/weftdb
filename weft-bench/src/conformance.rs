//! GPU conformance matrix (roadmap 5.6): every spline method on every backend and adapter,
//! measured against an exact reference and against splimes' published bound.
//!
//! splimes publishes one numerical contract for every backend and precision:
//!
//! `|v − exact| ≤ bound · range · Λ(t) + ε · |exact|`
//!
//! with `bound` = 1e-13 in `f64` (`Cpu`, `Parallel`, `Gpu`) and 1e-5 in `f32` (`Gpu` only),
//! `range` the spread of the input values, `Λ(t)` the Lebesgue function of the window used at
//! `t` and `ε = 2⁻⁵²`. splimes enforces it in its own test suite. This module measures it from
//! WeftDB's side, on the adapters this machine actually has, so WeftDB can publish what it
//! guarantees and what was observed (`docs/gpu-conformance.md`):
//!
//! - **Accuracy.** Each target (`Cpu`, `Parallel`, `Gpu` `f64`, `Gpu` `f32`, through both
//!   `Interpolator::run`, the server's `BigDecimal` path, and `run_f64`) runs each of six
//!   methods over a fixed corpus: synthetic edge cases (spacings from microseconds to 30
//!   days, values tiny, huge and large-offset, fewer points than the method needs, constant
//!   and stepped series, duplicate instants, a dense burst, geometric gaps) and a series in
//!   the shape of the UCI household-power corpus (minute cadence with missing runs), or the
//!   real file when given. Every grid point's error is taken against an exact 60-digit
//!   reference ([`exact_with_lebesgue`], a port of splimes' `tests/common`) and recorded in
//!   units of `range · Λ(t)`, the unit the bound is published in, as a maximum and a mean,
//!   inside and outside the data.
//! - **Run-to-run determinism.** Each target runs the corpus `repeats` times; the outputs are
//!   compared bit for bit.
//! - **Cross-target drift.** Every pair of targets (across adapters too) is compared point by
//!   point: the share of bit-identical values and the largest difference, in the same unit.
//!
//! The GPU is a process-wide singleton in splimes, so one process measures one adapter. The
//! `weft-gpu-conformance` binary runs one child per adapter environment (the default adapter,
//! and lavapipe when its Vulkan ICD is installed) and merges their results with
//! [`build_matrix`].

use std::{
	collections::BTreeMap, io::{Read, Write}, time::Instant
};

use bigdecimal::BigDecimal;
use chrono::{DateTime, TimeDelta, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use splimes::{Backend, Interpolator, Point, PointKind, Precision, Resolution, Spline};

/// Version of the run and matrix JSON layout.
pub const CONFORMANCE_SCHEMA_VERSION: u32 = 1;

/// The published `f64` bound (`Cpu`, `Parallel`, `Gpu` in `f64`), in units of `range · Λ`.
pub const BOUND_F64: f64 = 1e-13;
/// The published `f32` bound (`Gpu` in `f32`), in units of `range · Λ`.
pub const BOUND_F32: f64 = 1e-5;

/// The six methods measured: every `Spline` kind, with a bounded and an unbounded polynomial.
pub const SPLINES: [Spline; 6] = [Spline::Linear, Spline::Quadratic, Spline::Cubic, Spline::Polynomial(4, Some(0.5)), Spline::Polynomial(5, None), Spline::Polynomial(8, None)];

/// How much of the corpus to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CorpusSize {
	/// A handful of series on 32-point grids: seconds, for tests and smoke runs.
	Smoke,
	/// The whole corpus on 256-point grids: the measurement.
	Full,
}

impl CorpusSize {
	/// Grid points per fixture.
	#[must_use]
	pub const fn grid_points(self) -> i32 {
		match self {
			Self::Smoke => 32,
			Self::Full => 256,
		}
	}

	/// The name used on the command line and in reports.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Smoke => "smoke",
			Self::Full => "full",
		}
	}
}

/// One input series of the corpus.
#[derive(Debug, Clone)]
pub struct Series {
	/// `family/detail`, stable across runs.
	pub label: String,
	/// The input points, possibly unsorted or with duplicate instants.
	pub points: Vec<Point>,
}

/// xorshift64*: deterministic and dependency-free (the generator splimes' tests use).
struct Rng(u64);

impl Rng {
	const fn new(seed: u64) -> Self {
		Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
	}

	const fn next_u64(&mut self) -> u64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}

	/// Uniform in `[0, 1)`.
	#[allow(clippy::cast_precision_loss)]
	fn unit(&mut self) -> f64 {
		(self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
	}
}

/// The corpus' time origin: day 20,000 after the Unix epoch (2024-10-04).
#[must_use]
pub fn epoch() -> DateTime<Utc> {
	DateTime::<Utc>::UNIX_EPOCH + TimeDelta::days(20_000)
}

fn decimal(value: f64) -> BigDecimal {
	format!("{value:e}").parse().expect("a finite f64 prints as a decimal")
}

/// A random walk of `n` points, `spacing` nanoseconds apart (0.05× to 3× that when
/// `irregular`), shaped by `shape`: `walk` (unit steps), `offset` (1e9 plus a 1e-3 walk, the
/// case that breaks naive single precision), `tiny` (×1e-200) or `huge` (×1e200).
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn walk(rng: &mut Rng, n: usize, spacing: i64, shape: &str, irregular: bool) -> Vec<Point> {
	let mut t = epoch();
	let mut v = 0.0_f64;
	(0..n).map(|_| {
		let gap = if irregular { (spacing as f64 * 2.95_f64.mul_add(rng.unit(), 0.05)) as i64 } else { spacing };
		t += TimeDelta::nanoseconds(gap.max(1));
		v += rng.unit() - 0.5;
		let value = match shape {
			"offset" => v.mul_add(1e-3, 1e9),
			"tiny" => v * 1e-200,
			"huge" => v * 1e200,
			_ => v,
		};
		Point::new(t, decimal(value))
	})
	.collect()
}

/// A series in the shape of the UCI household-power corpus' `Global_active_power` column: one
/// reading a minute, three decimals of kW, a base load near 0.3 kW with appliance plateaus up
/// to ~8 kW, and runs of missing minutes (the real file is ~1.25% missing, in runs).
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
fn uci_shape(rng: &mut Rng, n: usize) -> Vec<Point> {
	let mut t = epoch();
	let mut level = 0.3_f64;
	let mut plateau = 0usize;
	let mut out = Vec::with_capacity(n);
	while out.len() < n {
		// A missing run: 1 in 60 readings is followed by 2 to 90 absent minutes.
		let skip = if rng.unit() < 1.0 / 60.0 { 2 + (rng.unit() * 88.0) as i64 } else { 1 };
		t += TimeDelta::minutes(skip);
		if plateau == 0 {
			// Appliances switch on for 5 to 60 minutes, otherwise the base load wanders.
			if rng.unit() < 0.04 {
				level = rng.unit().mul_add(7.0, 1.0);
				plateau = 5 + (rng.unit() * 55.0) as usize;
			} else {
				level = rng.unit().mul_add(0.3, 0.2);
			}
		} else {
			plateau -= 1;
		}
		let reading = (rng.unit() - 0.5).mul_add(0.05, level).max(0.076);
		// Three decimals, as the file stores them.
		let thousandths = (reading * 1000.0).round() as i64;
		out.push(Point::new(t, BigDecimal::new(thousandths.into(), 3)));
	}
	out
}

/// Turns a loaded UCI column into input points (epoch seconds → instants).
#[must_use]
pub fn uci_points(corpus: &crate::uci::UciCorpus) -> Vec<Point> {
	corpus.timestamps.iter().zip(&corpus.values).filter_map(|(&secs, value)| DateTime::<Utc>::from_timestamp(secs, 0).map(|t| Point::new(t, value.clone()))).collect()
}

/// The fixed corpus. `real` is the UCI file's points when the caller loaded it.
#[must_use]
pub fn corpus(size: CorpusSize, real: Option<Vec<Point>>) -> Vec<Series> {
	let mut rng = Rng::new(0x5EED_C0DE);
	let mut out = Vec::new();
	let mut push = |label: String, points: Vec<Point>| out.push(Series { label, points });
	let spacings: &[(&str, i64)] = match size {
		CorpusSize::Smoke => &[("s", 1_000_000_000)],
		CorpusSize::Full => &[("us", 1_000), ("s", 1_000_000_000), ("h", 3_600_000_000_000), ("30d", 2_592_000_000_000_000)],
	};
	let shapes: &[&str] = match size {
		CorpusSize::Smoke => &["walk", "offset"],
		CorpusSize::Full => &["walk", "offset", "tiny", "huge"],
	};
	for &(spacing_name, spacing) in spacings {
		for &shape in shapes {
			push(format!("walk/{spacing_name}/{shape}/irregular"), walk(&mut rng, 40, spacing, shape, true));
		}
	}
	push("walk/s/walk/regular".to_owned(), walk(&mut rng, 40, 1_000_000_000, "walk", false));
	// Fewer distinct points than most methods need: splimes steps down, and says so.
	let few: &[usize] = match size {
		CorpusSize::Smoke => &[2],
		CorpusSize::Full => &[1, 2, 3, 4],
	};
	for &n in few {
		push(format!("few/{n}"), walk(&mut rng, n, 1_000_000_000, "walk", true));
	}
	// A constant series: every method must return the constant.
	let t0 = epoch();
	push("constant".to_owned(), (1..=20).map(|i| Point::new(t0 + TimeDelta::seconds(i * 7), decimal(42.5))).collect());
	if size == CorpusSize::Full {
		// Steps between three levels: overshoot is the method's, not rounding.
		push("step".to_owned(), (1..=60).map(|i| Point::new(t0 + TimeDelta::seconds(i * 10), decimal([0.0, 5.0, -3.0][(i as usize / 20) % 3]))).collect());
		// Duplicate instants, shuffled: the last of each instant wins (the input contract).
		let mut dup = walk(&mut rng, 30, 1_000_000_000, "walk", true);
		let copies: Vec<Point> = dup.iter().step_by(3).map(|p| Point::new(p.timestamp, &p.value + BigDecimal::from(1))).collect();
		dup.extend(copies);
		dup.reverse();
		push("duplicates/shuffled".to_owned(), dup);
		push("walk/s/walk/long".to_owned(), walk(&mut rng, 2_000, 1_000_000_000, "walk", true));
	}
	// The contract's two deterministic stress shapes, both within "neighbouring gaps vary by
	// up to 60×".
	#[allow(clippy::cast_possible_truncation)]
	let at = |secs: f64| epoch() + TimeDelta::nanoseconds((secs * 1e9) as i64);
	let mut t = 0.0;
	push(
		"burst/smooth".to_owned(),
		(0..40).map(|i| {
			t += if i < 32 { 3.0 } else { 0.05 };
			Point::new(at(t), decimal((t / 10.0).sin().mul_add(10.0, t)))
		})
		.collect(),
	);
	let mut t = 0.0;
	push(
		"geometric/alternating".to_owned(),
		[0.0, 216_000.0, 3_600.0, 60.0, 1.0, 33.9, 1.0, 1.0, 1.0]
			.iter()
			.enumerate()
			.map(|(i, gap)| {
				t += gap;
				Point::new(at(t), decimal(if i % 2 == 0 { 1.0 } else { -1.0 }))
			})
			.collect(),
	);
	push("uci-shape/minute".to_owned(), uci_shape(&mut rng, if size == CorpusSize::Smoke { 200 } else { 1_440 }));
	if let Some(points) = real {
		push("uci/real".to_owned(), points);
	}
	out
}

/// `t` in nanoseconds since the Unix epoch on the POSIX scale.
fn posix(t: DateTime<Utc>) -> i128 {
	i128::from(t.timestamp()) * 1_000_000_000 + i128::from(t.timestamp_subsec_nanos())
}

/// Sorted by instant, last of each duplicate instant kept: splimes' documented input contract.
#[must_use]
pub fn distinct(points: &[Point]) -> Vec<Point> {
	let mut sorted = points.to_vec();
	// Stable, so the last of equal instants stays last.
	sorted.sort_by_key(|p| posix(p.timestamp));
	let mut out: Vec<Point> = Vec::with_capacity(sorted.len());
	for p in sorted {
		match out.last_mut() {
			Some(last) if posix(last.timestamp) == posix(p.timestamp) => *last = p,
			_ => out.push(p),
		}
	}
	out
}

fn value_range(points: &[Point]) -> (BigDecimal, BigDecimal) {
	let min = points.iter().map(|p| &p.value).min().expect("non-empty").clone();
	let max = points.iter().map(|p| &p.value).max().expect("non-empty").clone();
	(min, max)
}

/// The nearest `f64` to a decimal (its shortest round-trip text parses back exactly).
#[must_use]
pub fn to_f64(d: &BigDecimal) -> f64 {
	d.to_string().parse().expect("a decimal parses as f64")
}

/// The method defined on `Spline`, evaluated at `t` in exact decimal arithmetic.
///
/// The arithmetic carries 60 significant digits per term, on the caller's own timestamps, each value first rounded to
/// its nearest `f64` as splimes does; and `Λ(t)`, the window's Lebesgue function there (at
/// least 1).
///
/// A port of splimes' `tests/common::exact_with_lebesgue`, the reference its
/// contract tests use.
///
/// # Panics
///
/// On an empty series or a non-finite value.
#[must_use]
pub fn exact_with_lebesgue(points: &[Point], spline: Spline, t: DateTime<Utc>) -> (BigDecimal, f64) {
	let knots: Vec<Point> = distinct(points).into_iter().map(|p| Point::new(p.timestamp, BigDecimal::try_from(to_f64(&p.value)).expect("finite"))).collect();
	let n = knots.len();
	let spline = spline.fallback_for(n);
	let x: Vec<BigDecimal> = knots.iter().map(|p| BigDecimal::from(posix(p.timestamp) - posix(knots[0].timestamp))).collect();
	let target = BigDecimal::from(posix(t) - posix(knots[0].timestamp));
	let below = target < x[0];
	let above = target > x[n - 1];
	if matches!(spline, Spline::Cubic) && (below || above) {
		return (if below { knots[0].value.clone() } else { knots[n - 1].value.clone() }, 1.0);
	}
	let m = spline.min_points().min(n);
	let p = x.iter().filter(|&xi| *xi <= target).count();
	#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
	let start = (p as isize - (m / 2) as isize).clamp(0, (n - m) as isize) as usize;
	let mut total = BigDecimal::from(0);
	let mut lebesgue = BigDecimal::from(0);
	for j in start..start + m {
		let mut basis = BigDecimal::from(1);
		for k in start..start + m {
			if k != j {
				basis = (basis * (&target - &x[k]) / (&x[j] - &x[k])).with_prec(60);
			}
		}
		total += (&knots[j].value * &basis).with_prec(60);
		lebesgue += basis.abs();
	}
	if let (Some(b), true) = (spline.bounds_factor(), below || above) {
		let (min, max) = value_range(&knots);
		let range = &max - &min;
		let b: BigDecimal = b.to_string().parse().expect("finite bounds factor");
		let lo = &min - &range * &b;
		let hi = &max + &range * &b;
		total = total.max(lo).min(hi);
	}
	(total, to_f64(&lebesgue).max(1.0))
}

/// One grid of one series under one method, with the exact answer at every grid point.
#[derive(Debug, Clone)]
pub struct Fixture {
	/// Index into the corpus.
	pub series: usize,
	/// The method.
	pub spline: Spline,
	/// Which grid of the series: `before`, `middle`, `densest`, `after`, `far`.
	pub grid: &'static str,
	/// Grid start (inclusive).
	pub start: DateTime<Utc>,
	/// Grid end (inclusive).
	pub end: DateTime<Utc>,
	/// Grid step.
	pub resolution: Resolution,
	/// The exact value at each grid point, as the nearest `f64`.
	pub exact: Vec<f64>,
	/// `range · Λ(t)` at each grid point: the unit errors are measured in.
	pub scale: Vec<f64>,
}

/// Builds every fixture of the corpus.
///
/// This is per series, five grids (across the first edge, the
/// middle, the densest stretch, across the last edge, and 100 edge spacings out) for each
/// method, as splimes' contract test lays them out.
///
/// The exact reference runs on rayon.
///
/// # Panics
///
/// On an empty series.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub fn fixtures(corpus: &[Series], size: CorpusSize) -> Vec<Fixture> {
	let grid_points = size.grid_points();
	let mut jobs = Vec::new();
	for (index, series) in corpus.iter().enumerate() {
		let knots = distinct(&series.points);
		let n = knots.len();
		let o: Vec<i128> = knots.iter().map(|p| posix(p.timestamp) - posix(knots[0].timestamp)).collect();
		let first = knots[0].timestamp;
		let last = knots[n - 1].timestamp;
		// The coarsest resolution with at least 20 grid steps across the smallest knot gap.
		let min_gap = o.windows(2).map(|w| w[1] - w[0]).min().unwrap_or(1_000_000_000).max(20);
		let resolution = Resolution::ALL.iter().copied().rev().find(|r| i128::from(r.step_nanos()) * 20 <= min_gap).unwrap_or(Resolution::Nanoseconds);
		let width = resolution.step() * (grid_points - 1);
		let span = o[n - 1].max(1_000);
		let middle = first + TimeDelta::nanoseconds((span / 2) as i64);
		let densest = o.windows(2).enumerate().min_by_key(|(_, w)| w[1] - w[0]).map_or(first, |(i, _)| knots[i].timestamp);
		let (min, max) = value_range(&knots);
		let range = to_f64(&(&max - &min));
		let range = if range > 0.0 { range } else { to_f64(&max).abs().max(f64::MIN_POSITIVE) };
		for spline in SPLINES {
			let m = spline.fallback_for(n).min_points().min(n);
			let edge_pad = |a: i128, b: i128| TimeDelta::nanoseconds(if m >= 2 { (2 * (b - a) / (m as i128 - 1)) as i64 } else { 2_000 });
			let (pad_start, pad_end) = (edge_pad(o[0], o[m - 1]), edge_pad(o[n - m], o[n - 1]));
			let far = last + pad_end * 50;
			for (grid, start) in [("before", first - pad_start), ("middle", middle), ("densest", densest), ("after", last + pad_end - width), ("far", far)] {
				jobs.push((index, spline, grid, start, start + width, resolution, range));
			}
		}
	}
	jobs.into_par_iter()
		.map(|(index, spline, grid, start, end, resolution, range)| {
			let (exact, scale) = (0..grid_points)
				.map(|k| {
					let (value, lebesgue) = exact_with_lebesgue(&corpus[index].points, spline, start + resolution.step() * k);
					(to_f64(&value), range * lebesgue)
				})
				.unzip();
			Fixture { series: index, spline, grid, start, end, resolution, exact, scale }
		})
		.collect()
}

/// A hash of the corpus and its fixtures (inputs, grids, exact values), so results from
/// different processes are known to describe the same corpus.
#[must_use]
pub fn corpus_hash(corpus: &[Series], fixtures: &[Fixture]) -> String {
	let mut h = Sha256::new();
	for s in corpus {
		h.update(s.label.as_bytes());
		for p in &s.points {
			h.update(posix(p.timestamp).to_le_bytes());
			h.update(p.value.to_string().as_bytes());
		}
	}
	for f in fixtures {
		h.update(f.series.to_le_bytes());
		h.update(f.spline.to_string().as_bytes());
		h.update(posix(f.start).to_le_bytes());
		h.update(posix(f.end).to_le_bytes());
		for (e, s) in f.exact.iter().zip(&f.scale) {
			h.update(e.to_bits().to_le_bytes());
			h.update(s.to_bits().to_le_bytes());
		}
	}
	h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where a run executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
	/// `Backend::Cpu`.
	Cpu,
	/// `Backend::Parallel`.
	Parallel,
	/// `Backend::Gpu`, `Precision::F64`.
	GpuF64,
	/// `Backend::Gpu`, `Precision::F32`.
	GpuF32,
}

impl Target {
	/// Every target, CPU first.
	pub const ALL: [Self; 4] = [Self::Cpu, Self::Parallel, Self::GpuF64, Self::GpuF32];

	/// The name used in reports.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Cpu => "cpu",
			Self::Parallel => "parallel",
			Self::GpuF64 => "gpu-f64",
			Self::GpuF32 => "gpu-f32",
		}
	}

	/// The published bound for this target, in units of `range · Λ`.
	#[must_use]
	pub const fn bound(self) -> f64 {
		match self {
			Self::GpuF32 => BOUND_F32,
			_ => BOUND_F64,
		}
	}

	/// Whether the target runs on the GPU.
	#[must_use]
	pub const fn is_gpu(self) -> bool {
		matches!(self, Self::GpuF64 | Self::GpuF32)
	}

	const fn backend(self) -> (Backend, Precision) {
		match self {
			Self::Cpu => (Backend::Cpu, Precision::F64),
			Self::Parallel => (Backend::Parallel, Precision::F64),
			Self::GpuF64 => (Backend::Gpu, Precision::F64),
			Self::GpuF32 => (Backend::Gpu, Precision::F32),
		}
	}
}

/// Which splimes entry point a run goes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
	/// `Interpolator::run` on `BigDecimal` points: the path `weft-server` takes.
	Decimal,
	/// `Interpolator::run_f64` on `f64` columns: `interpolate_range`'s float path.
	F64,
}

impl Api {
	/// Both entry points.
	pub const ALL: [Self; 2] = [Self::Decimal, Self::F64];

	/// The name used in reports.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Decimal => "decimal",
			Self::F64 => "f64",
		}
	}
}

/// What one fixture produced on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
	/// The grid values (`f64` bits) and their kinds.
	Values {
		/// `f64::to_bits` of each grid value.
		bits: Vec<u64>,
		/// Each grid point's provenance.
		kinds: Vec<PointKind>,
		/// Points the `f32` kernel handed to `f64`.
		recomputed_in_f64: usize,
		/// Raw points whose value differs from the input (must be 0).
		raw_mismatches: usize,
	},
	/// `Error::NonFiniteResult`; `justified` when the exact value really leaves `f64`.
	NonFinite {
		/// Whether the exact value at the reported instant is beyond `f64`.
		justified: bool,
	},
	/// Any other error (a conformance failure).
	Error(String),
}

/// Runs every fixture once on one target through one entry point.
///
/// # Panics
///
/// Never on a well-formed corpus.
#[must_use]
pub fn run_once(corpus: &[Series], fixtures: &[Fixture], target: Target, api: Api) -> Vec<Outcome> {
	let (backend, precision) = target.backend();
	fixtures.iter()
		.map(|f| {
			let points = &corpus[f.series].points;
			let interpolator = Interpolator::new(f.spline, f.resolution).backend(backend).gpu_precision(precision);
			let justified = |timestamp| {
				let (exact, _) = exact_with_lebesgue(points, f.spline, timestamp);
				let exact = to_f64(&exact);
				!exact.is_finite() || exact.abs() > 0.999 * f64::MAX
			};
			match api {
				Api::Decimal => match interpolator.run(points, f.start, f.end) {
					Ok(r) => {
						let knots = distinct(points);
						let raw_mismatches = r.timestamps().iter().zip(r.values()).zip(r.kinds()).filter(|((t, v), k)| **k == PointKind::Raw && knots.iter().find(|p| p.timestamp == **t).is_none_or(|p| &p.value != *v)).count();
						Outcome::Values { bits: r.values().iter().map(|v| to_f64(v).to_bits()).collect(), kinds: r.kinds().to_vec(), recomputed_in_f64: r.points_recomputed_in_f64(), raw_mismatches }
					}
					Err(splimes::Error::NonFiniteResult { timestamp }) => Outcome::NonFinite { justified: justified(timestamp) },
					Err(e) => Outcome::Error(e.to_string()),
				},
				Api::F64 => {
					let ts: Vec<_> = points.iter().map(|p| p.timestamp).collect();
					let vs: Vec<_> = points.iter().map(|p| to_f64(&p.value)).collect();
					match interpolator.run_f64(&ts, &vs, f.start, f.end) {
						Ok(r) => {
							let knots = distinct(points);
							let raw_mismatches = r.timestamps().iter().zip(r.values()).zip(r.kinds()).filter(|((t, v), k)| **k == PointKind::Raw && knots.iter().find(|p| p.timestamp == **t).is_none_or(|p| to_f64(&p.value).to_bits() != v.to_bits())).count();
							Outcome::Values { bits: r.values().iter().map(|v| v.to_bits()).collect(), kinds: r.kinds().to_vec(), recomputed_in_f64: r.points_recomputed_in_f64(), raw_mismatches }
						}
						Err(splimes::Error::NonFiniteResult { timestamp }) => Outcome::NonFinite { justified: justified(timestamp) },
						Err(e) => Outcome::Error(e.to_string()),
					}
				}
			}
		})
		.collect()
}

/// Error statistics for one method on one target, in units of `range · Λ`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MethodStats {
	/// The method, as `Spline`'s `Display` writes it.
	pub method: String,
	/// Interpolated grid points measured.
	pub inside_points: u64,
	/// Largest interpolation error.
	pub inside_max: f64,
	/// Mean interpolation error.
	pub inside_mean: f64,
	/// Extrapolated grid points measured.
	pub outside_points: u64,
	/// Largest extrapolation error.
	pub outside_max: f64,
	/// Mean extrapolation error.
	pub outside_mean: f64,
	/// Raw points (exact by contract) seen.
	pub raw_points: u64,
	/// Raw points whose value was not the input's, exactly (must be 0).
	pub raw_mismatches: u64,
	/// Points the `f32` kernel recomputed in `f64`.
	pub recomputed_in_f64: u64,
	/// Fixtures that returned `NonFiniteResult` where the exact value leaves `f64` (correct).
	pub nonfinite_justified: u64,
	/// Fixtures that returned `NonFiniteResult` for a finite exact value (a failure).
	pub nonfinite_unjustified: u64,
	/// Fixtures that failed with another error (a failure), with the first message.
	pub errors: u64,
	/// The first error message, if any.
	pub first_error: Option<String>,
	/// The published bound.
	pub bound: f64,
	/// Whether every measured error is within the bound and nothing failed.
	pub pass: bool,
}

/// The contract error of `got` against `exact`: `(|got − exact| − ε·|exact|)⁺ / (range · Λ)`.
#[must_use]
pub fn contract_error(got: f64, exact: f64, scale: f64) -> f64 {
	f64::mul_add(f64::EPSILON, -exact.abs(), (got - exact).abs()).max(0.0) / scale
}

/// Scores one run against the exact reference, per method.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn score(fixtures: &[Fixture], outcomes: &[Outcome], target: Target) -> Vec<MethodStats> {
	let mut rows: Vec<(MethodStats, f64, f64)> = SPLINES.iter().map(|s| (MethodStats { method: s.to_string(), bound: target.bound(), ..MethodStats::default() }, 0.0, 0.0)).collect();
	for (f, outcome) in fixtures.iter().zip(outcomes) {
		let (row, inside_sum, outside_sum) = &mut rows[SPLINES.iter().position(|s| *s == f.spline).expect("a measured method")];
		match outcome {
			Outcome::Values { bits, kinds, recomputed_in_f64, raw_mismatches } => {
				row.recomputed_in_f64 += *recomputed_in_f64 as u64;
				row.raw_mismatches += *raw_mismatches as u64;
				if bits.len() != f.exact.len() {
					row.errors += 1;
					row.first_error.get_or_insert_with(|| format!("grid of {} points, expected {}", bits.len(), f.exact.len()));
					continue;
				}
				for (((&b, kind), &exact), &scale) in bits.iter().zip(kinds).zip(&f.exact).zip(&f.scale) {
					let got = f64::from_bits(b);
					// A NaN or infinity returned as a value is a failure the bound can't express.
					let err = if got.is_finite() { contract_error(got, exact, scale) } else { f64::INFINITY };
					match kind {
						PointKind::Raw => row.raw_points += 1,
						PointKind::Interpolated => {
							row.inside_points += 1;
							*inside_sum += err;
							row.inside_max = row.inside_max.max(err);
						}
						PointKind::Extrapolated => {
							row.outside_points += 1;
							*outside_sum += err;
							row.outside_max = row.outside_max.max(err);
						}
					}
				}
			}
			Outcome::NonFinite { justified: true } => row.nonfinite_justified += 1,
			Outcome::NonFinite { justified: false } => row.nonfinite_unjustified += 1,
			Outcome::Error(e) => {
				row.errors += 1;
				row.first_error.get_or_insert_with(|| e.clone());
			}
		}
	}
	rows.into_iter()
		.map(|(mut row, inside_sum, outside_sum)| {
			row.inside_mean = if row.inside_points > 0 { inside_sum / row.inside_points as f64 } else { 0.0 };
			row.outside_mean = if row.outside_points > 0 { outside_sum / row.outside_points as f64 } else { 0.0 };
			row.pass = row.inside_max <= row.bound && row.outside_max <= row.bound && row.raw_mismatches == 0 && row.nonfinite_unjustified == 0 && row.errors == 0;
			row
		})
		.collect()
}

/// The values of a run, by fixture (`None` where the fixture returned no grid).
#[must_use]
pub fn values_of(outcomes: &[Outcome]) -> Vec<Option<Vec<u64>>> {
	outcomes.iter()
		.map(|o| match o {
			Outcome::Values { bits, .. } => Some(bits.clone()),
			_ => None,
		})
		.collect()
}

/// Point-by-point difference between two runs of the same fixtures.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Drift {
	/// Grid points both runs returned.
	pub points: u64,
	/// Of those, how many are bit-identical.
	pub identical: u64,
	/// Fixtures one run returned a grid for and the other did not.
	pub outcome_mismatches: u64,
	/// Largest `|a − b| / (range · Λ)`.
	pub max: f64,
	/// Mean `|a − b| / (range · Λ)`.
	pub mean: f64,
}

/// Compares two runs' values point by point.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn drift(fixtures: &[Fixture], a: &[Option<Vec<u64>>], b: &[Option<Vec<u64>>]) -> Drift {
	let mut d = Drift::default();
	let mut sum = 0.0;
	for ((f, a), b) in fixtures.iter().zip(a).zip(b) {
		match (a, b) {
			(Some(a), Some(b)) if a.len() == b.len() => {
				for ((&x, &y), &scale) in a.iter().zip(b).zip(&f.scale) {
					d.points += 1;
					if x == y {
						d.identical += 1;
						continue;
					}
					let diff = (f64::from_bits(x) - f64::from_bits(y)).abs() / scale;
					let diff = if diff.is_nan() { f64::INFINITY } else { diff };
					sum += diff;
					d.max = d.max.max(diff);
				}
			}
			(None, None) => {}
			_ => d.outcome_mismatches += 1,
		}
	}
	d.mean = if d.points > 0 { sum / d.points as f64 } else { 0.0 };
	d
}

/// The adapter a GPU target ran on (splimes' `GpuInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adapter {
	/// Adapter name.
	pub name: String,
	/// Graphics API.
	pub api: String,
	/// `discrete`, `integrated`, `virtual`, `cpu` or `other`.
	pub device_type: String,
	/// Driver name and version.
	pub driver: String,
	/// Whether the `f64` kernel is available.
	pub supports_f64: bool,
}

/// One target through one entry point in one environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetReport {
	/// The target.
	pub target: Target,
	/// The entry point.
	pub api: Api,
	/// Why the target did not run, if it did not (no GPU, no `f64` kernel).
	pub unavailable: Option<String>,
	/// Per-method error statistics against the exact reference (first repeat).
	pub methods: Vec<MethodStats>,
	/// Times the corpus ran.
	pub repeats: usize,
	/// Whether every repeat returned bit-identical values and outcomes.
	pub run_to_run_identical: bool,
	/// Drift of the worst repeat against the first.
	pub run_to_run: Drift,
	/// Wall time of the first repeat, in milliseconds (informational, not a benchmark).
	pub wall_ms: f64,
}

impl TargetReport {
	/// `env/target/api`, the key used in the matrix.
	#[must_use]
	pub fn key(&self, env: &str) -> String {
		format!("{env}/{}/{}", self.target.as_str(), self.api.as_str())
	}

	/// Whether the target ran and kept the contract for every method.
	#[must_use]
	pub fn pass(&self) -> bool {
		self.unavailable.is_some() || self.methods.iter().all(|m| m.pass)
	}
}

/// Everything one process measured: one adapter environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentRun {
	/// [`CONFORMANCE_SCHEMA_VERSION`].
	pub schema_version: u32,
	/// The environment's name (`host`, `lavapipe`, …).
	pub env: String,
	/// Environment variables the environment was started with.
	pub env_vars: BTreeMap<String, String>,
	/// Corpus size.
	pub corpus_size: CorpusSize,
	/// Series in the corpus.
	pub series: Vec<String>,
	/// Fixtures (grids) per target.
	pub fixtures: usize,
	/// Grid points per target.
	pub grid_points: usize,
	/// [`corpus_hash`].
	pub corpus_hash: String,
	/// The GPU adapter, when the environment has one.
	pub adapter: Option<Adapter>,
	/// Why the GPU did not start, when it did not.
	pub gpu_error: Option<String>,
	/// When the run started.
	pub started_at: DateTime<Utc>,
	/// Per target and entry point.
	pub targets: Vec<TargetReport>,
}

/// Options for one environment's run.
#[derive(Debug, Clone)]
pub struct RunOptions {
	/// Environment name.
	pub env: String,
	/// Environment variables it was started with (recorded only).
	pub env_vars: BTreeMap<String, String>,
	/// Corpus size.
	pub size: CorpusSize,
	/// Repeats per target (at least 1).
	pub repeats: usize,
	/// Targets to run.
	pub targets: Vec<Target>,
	/// The real UCI points, when loaded.
	pub real: Option<Vec<Point>>,
}

/// Runs one environment: builds the corpus and fixtures, starts the GPU if a GPU target is
/// asked for, runs every target through both entry points `repeats` times and scores them.
///
/// Returns the report and each target's values (first repeat) for drift.
#[must_use]
pub fn run_environment(options: &RunOptions) -> (EnvironmentRun, Vec<Vec<Option<Vec<u64>>>>) {
	let started_at = Utc::now();
	let corpus = corpus(options.size, options.real.clone());
	let fixtures = fixtures(&corpus, options.size);
	let hash = corpus_hash(&corpus, &fixtures);
	let (adapter, gpu_error) = if options.targets.iter().any(|t| t.is_gpu()) {
		match splimes::prewarm_gpu() {
			Ok(info) => (Some(Adapter { name: info.name, api: info.api, device_type: info.device_type, driver: info.driver, supports_f64: info.supports_f64 }), None),
			Err(e) => (None, Some(e.to_string())),
		}
	} else {
		(None, None)
	};
	let mut reports = Vec::new();
	let mut values = Vec::new();
	for &target in &options.targets {
		for api in Api::ALL {
			let unavailable = match (target.is_gpu(), &adapter) {
				(false, _) => None,
				(true, None) => Some(format!("no GPU: {}", gpu_error.as_deref().unwrap_or("not started"))),
				(true, Some(a)) if target == Target::GpuF64 && !a.supports_f64 => Some(format!("{} has no f64 kernel", a.name)),
				(true, Some(_)) => None,
			};
			if let Some(reason) = unavailable {
				reports.push(TargetReport { target, api, unavailable: Some(reason), methods: Vec::new(), repeats: 0, run_to_run_identical: true, run_to_run: Drift::default(), wall_ms: 0.0 });
				values.push(vec![None; fixtures.len()]);
				continue;
			}
			let clock = Instant::now();
			let first = run_once(&corpus, &fixtures, target, api);
			let wall_ms = clock.elapsed().as_secs_f64() * 1e3;
			let first_values = values_of(&first);
			let mut worst = Drift::default();
			let mut identical = true;
			for _ in 1..options.repeats.max(1) {
				let again = run_once(&corpus, &fixtures, target, api);
				identical &= again == first;
				let d = drift(&fixtures, &first_values, &values_of(&again));
				if d.points - d.identical > worst.points - worst.identical || d.outcome_mismatches > worst.outcome_mismatches || worst.points == 0 {
					worst = d;
				}
			}
			reports.push(TargetReport { target, api, unavailable: None, methods: score(&fixtures, &first, target), repeats: options.repeats.max(1), run_to_run_identical: identical, run_to_run: worst, wall_ms });
			values.push(first_values);
		}
	}
	let report = EnvironmentRun { schema_version: CONFORMANCE_SCHEMA_VERSION, env: options.env.clone(), env_vars: options.env_vars.clone(), corpus_size: options.size, series: corpus.iter().map(|s| s.label.clone()).collect(), fixtures: fixtures.len(), grid_points: fixtures.iter().map(|f| f.exact.len()).sum(), corpus_hash: hash, adapter, gpu_error, started_at, targets: reports };
	(report, values)
}

const VALUES_MAGIC: &[u8; 8] = b"WGCVAL01";
const NO_GRID: u32 = u32::MAX;

/// Writes a run's values (one entry per target report, in order) as a little-endian binary
/// sidecar.
///
/// JSON would need `serde_json`'s `float_roundtrip` to read every value back exactly,
/// and would be ~10× larger.
///
/// # Errors
///
/// I/O errors.
pub fn write_values(mut w: impl Write, values: &[Vec<Option<Vec<u64>>>]) -> std::io::Result<()> {
	w.write_all(VALUES_MAGIC)?;
	w.write_all(&u32::try_from(values.len()).map_err(std::io::Error::other)?.to_le_bytes())?;
	for target in values {
		w.write_all(&u32::try_from(target.len()).map_err(std::io::Error::other)?.to_le_bytes())?;
		for fixture in target {
			match fixture {
				None => w.write_all(&NO_GRID.to_le_bytes())?,
				Some(bits) => {
					w.write_all(&u32::try_from(bits.len()).map_err(std::io::Error::other)?.to_le_bytes())?;
					for b in bits {
						w.write_all(&b.to_le_bytes())?;
					}
				}
			}
		}
	}
	Ok(())
}

/// Reads what [`write_values`] wrote.
///
/// # Errors
///
/// I/O errors, or a file that is not a values sidecar.
pub fn read_values(mut r: impl Read) -> std::io::Result<Vec<Vec<Option<Vec<u64>>>>> {
	let mut magic = [0u8; 8];
	r.read_exact(&mut magic)?;
	if &magic != VALUES_MAGIC {
		return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a conformance values file"));
	}
	let mut u32_buf = [0u8; 4];
	let mut read_u32 = |r: &mut dyn Read| -> std::io::Result<u32> {
		r.read_exact(&mut u32_buf)?;
		Ok(u32::from_le_bytes(u32_buf))
	};
	let targets = read_u32(&mut r)?;
	let mut out = Vec::new();
	for _ in 0..targets {
		let fixtures = read_u32(&mut r)?;
		let mut target = Vec::new();
		for _ in 0..fixtures {
			let len = read_u32(&mut r)?;
			if len == NO_GRID {
				target.push(None);
				continue;
			}
			let mut bits = Vec::with_capacity(len as usize);
			let mut buf = [0u8; 8];
			for _ in 0..len {
				r.read_exact(&mut buf)?;
				bits.push(u64::from_le_bytes(buf));
			}
			target.push(Some(bits));
		}
		out.push(target);
	}
	Ok(out)
}

/// One pair of targets in the drift table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftRow {
	/// `env/target/api` of the first.
	pub a: String,
	/// `env/target/api` of the second.
	pub b: String,
	/// The drift between them.
	pub drift: Drift,
}

/// The merged matrix of every environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Matrix {
	/// [`CONFORMANCE_SCHEMA_VERSION`].
	pub schema_version: u32,
	/// The environments merged.
	pub environments: Vec<EnvironmentRun>,
	/// Environments that could not run, with the reason.
	pub skipped: Vec<(String, String)>,
	/// Every pair of targets that ran, compared point by point.
	pub drift: Vec<DriftRow>,
	/// Contract failures found (empty when the matrix passes).
	pub failures: Vec<String>,
}

/// Merges environment runs (and their values) into the matrix, computing every pairwise drift
/// and listing every contract failure.
///
/// This is a bound exceeded, a raw point not exact, a wrong
/// `NonFiniteResult` or another error, `Cpu` and `Parallel` not bit-identical within an
/// environment (promised by the contract), or environments built from a different corpus
/// than `fixtures`.
///
#[must_use]
pub fn build_matrix(fixtures: &[Fixture], corpus_hash: &str, runs: Vec<(EnvironmentRun, Vec<Vec<Option<Vec<u64>>>>)>, skipped: Vec<(String, String)>) -> Matrix {
	let mut failures = Vec::new();
	let mut keyed: Vec<(String, &Vec<Option<Vec<u64>>>)> = Vec::new();
	for (run, values) in &runs {
		if run.corpus_hash != corpus_hash {
			failures.push(format!("{}: measured a different corpus ({} ≠ {corpus_hash})", run.env, run.corpus_hash));
			continue;
		}
		if values.len() != run.targets.len() {
			failures.push(format!("{}: {} value sets for {} targets", run.env, values.len(), run.targets.len()));
			continue;
		}
		for (report, v) in run.targets.iter().zip(values) {
			let key = report.key(&run.env);
			for m in report.methods.iter().filter(|m| !m.pass) {
				failures.push(format!("{key} {}: inside {:.2e}, outside {:.2e} (bound {:.0e}); raw mismatches {}; unjustified non-finite {}; errors {}{}", m.method, m.inside_max, m.outside_max, m.bound, m.raw_mismatches, m.nonfinite_unjustified, m.errors, m.first_error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()));
			}
			if report.unavailable.is_none() {
				if !report.target.is_gpu() && !report.run_to_run_identical {
					failures.push(format!("{key}: CPU results differ between repeats"));
				}
				keyed.push((key, v));
			}
		}
		for api in Api::ALL {
			let find = |t: Target| run.targets.iter().zip(values).find(|(r, _)| r.target == t && r.api == api && r.unavailable.is_none()).map(|(_, v)| v);
			if let (Some(cpu), Some(parallel)) = (find(Target::Cpu), find(Target::Parallel)) {
				let d = drift(fixtures, cpu, parallel);
				if d.identical != d.points || d.outcome_mismatches != 0 {
					failures.push(format!("{}/{}: Cpu and Parallel are not bit-identical ({} of {} points differ)", run.env, api.as_str(), d.points - d.identical, d.points));
				}
			}
		}
	}
	let mut drift_rows = Vec::new();
	for (i, (a, va)) in keyed.iter().enumerate() {
		for (b, vb) in &keyed[i + 1..] {
			drift_rows.push(DriftRow { a: a.clone(), b: b.clone(), drift: drift(fixtures, va, vb) });
		}
	}
	Matrix { schema_version: CONFORMANCE_SCHEMA_VERSION, environments: runs.into_iter().map(|(r, _)| r).collect(), skipped, drift: drift_rows, failures }
}

fn sci(x: f64) -> String {
	if x == 0.0 {
		"0".to_owned()
	} else if x.is_infinite() {
		"inf".to_owned()
	} else {
		format!("{x:.1e}")
	}
}

/// Renders the matrix as Markdown: environments, the accuracy table (worst and mean error per
/// method in units of `range · Λ`, against the bound), run-to-run determinism, and drift.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn render_markdown(matrix: &Matrix) -> String {
	use std::fmt::Write as _;
	let mut s = String::new();
	let _ = writeln!(s, "# GPU conformance matrix\n");
	let _ = writeln!(s, "Errors are `(|v − exact| − ε·|exact|)⁺ / (range · Λ(t))` against a 60-digit exact reference: the unit splimes publishes its bound in (1e-13 `f64`, 1e-5 `f32`).\n");
	let _ = writeln!(s, "## Environments\n");
	let _ = writeln!(s, "| Environment | Adapter | API | Type | Driver | f64 | Corpus | Grids | Points | Started |");
	let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|");
	for e in &matrix.environments {
		let (name, api, kind, driver, f64_ok) = e.adapter.as_ref().map_or_else(|| (e.gpu_error.clone().map_or_else(|| "(CPU only)".to_owned(), |err| format!("none: {err}")), String::new(), String::new(), String::new(), String::new()), |a| (a.name.clone(), a.api.clone(), a.device_type.clone(), a.driver.clone(), if a.supports_f64 { "yes".to_owned() } else { "no".to_owned() }));
		let _ = writeln!(s, "| {} | {name} | {api} | {kind} | {driver} | {f64_ok} | {} `{}` | {} | {} | {} |", e.env, e.corpus_size.as_str(), &e.corpus_hash[..12.min(e.corpus_hash.len())], e.fixtures, e.grid_points, e.started_at.format("%Y-%m-%d %H:%M UTC"));
	}
	for (env, why) in &matrix.skipped {
		let _ = writeln!(s, "| {env} | not measured: {why} | | | | | | | | |");
	}
	let _ = writeln!(s, "\n## Accuracy\n");
	let _ = writeln!(s, "| Target | Method | Inside max | Inside mean | Outside max | Outside mean | Bound | Headroom | f32→f64 | Non-finite | Pass |");
	let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|");
	for e in &matrix.environments {
		for t in &e.targets {
			let key = t.key(&e.env);
			if let Some(why) = &t.unavailable {
				let _ = writeln!(s, "| {key} | — | not run: {why} | | | | | | | | |");
				continue;
			}
			for m in &t.methods {
				let worst = m.inside_max.max(m.outside_max);
				let headroom = if worst > 0.0 { format!("{:.0}×", m.bound / worst) } else { "∞".to_owned() };
				let _ = writeln!(s, "| {key} | `{}` | {} | {} | {} | {} | {:.0e} | {headroom} | {} | {} | {} |", m.method, sci(m.inside_max), sci(m.inside_mean), sci(m.outside_max), sci(m.outside_mean), m.bound, m.recomputed_in_f64, m.nonfinite_justified, if m.pass { "yes" } else { "**no**" });
			}
		}
	}
	let _ = writeln!(s, "\n## Run-to-run determinism\n");
	let _ = writeln!(s, "| Target | Repeats | Bit-identical | Points differing (worst repeat) | Max drift |");
	let _ = writeln!(s, "|---|---|---|---|---|");
	for e in &matrix.environments {
		for t in e.targets.iter().filter(|t| t.unavailable.is_none()) {
			let _ = writeln!(s, "| {} | {} | {} | {} | {} |", t.key(&e.env), t.repeats, if t.run_to_run_identical { "yes" } else { "no" }, t.run_to_run.points - t.run_to_run.identical, sci(t.run_to_run.max));
		}
	}
	let _ = writeln!(s, "\n## Cross-target drift\n");
	let _ = writeln!(s, "| A | B | Points | Bit-identical | Max | Mean |");
	let _ = writeln!(s, "|---|---|---|---|---|---|");
	for row in &matrix.drift {
		let share = if row.drift.points > 0 { row.drift.identical as f64 / row.drift.points as f64 * 100.0 } else { 0.0 };
		let _ = writeln!(s, "| {} | {} | {} | {share:.2}% | {} | {} |", row.a, row.b, row.drift.points, sci(row.drift.max), sci(row.drift.mean));
	}
	let _ = writeln!(s, "\n## Verdict\n");
	if matrix.failures.is_empty() {
		let _ = writeln!(s, "Every target that ran kept the published bound, every raw point was exact, and `Cpu` and `Parallel` were bit-identical.");
	} else {
		for f in &matrix.failures {
			let _ = writeln!(s, "- FAIL: {f}");
		}
	}
	s
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(secs: i64) -> DateTime<Utc> {
		epoch() + TimeDelta::seconds(secs)
	}

	fn pts(values: &[(i64, f64)]) -> Vec<Point> {
		values.iter().map(|&(t, v)| Point::new(at(t), decimal(v))).collect()
	}

	#[test]
	fn the_reference_is_the_methods_definition() {
		let p = pts(&[(0, 0.0), (10, 10.0), (20, 0.0), (30, 10.0)]);
		// Linear inside: the chord, with Λ = 1.
		let (v, l) = exact_with_lebesgue(&p, Spline::Linear, at(5));
		assert_eq!((to_f64(&v), l), (5.0, 1.0));
		// Cubic holds the edge value outside the data.
		assert_eq!(to_f64(&exact_with_lebesgue(&p, Spline::Cubic, at(-50)).0), 0.0);
		assert_eq!(to_f64(&exact_with_lebesgue(&p, Spline::Cubic, at(80)).0), 10.0);
		// A quadratic through (0,0), (10,10), (20,0) peaks at 10 at t = 10 and is 7.5 at 5.
		assert_eq!(to_f64(&exact_with_lebesgue(&p[..3], Spline::Quadratic, at(5)).0), 7.5);
		// A bounded polynomial far out is clamped to max + range · b.
		let (far, _) = exact_with_lebesgue(&p, Spline::Polynomial(3, Some(0.5)), at(1_000));
		assert_eq!(to_f64(&far), 15.0);
		// The last of duplicate instants wins.
		let dup = pts(&[(0, 1.0), (10, 2.0), (0, 3.0)]);
		assert_eq!(to_f64(&exact_with_lebesgue(&dup, Spline::Linear, at(0)).0), 3.0);
	}

	#[test]
	fn the_corpus_is_deterministic_and_covers_the_edge_cases() {
		let a = corpus(CorpusSize::Smoke, None);
		let b = corpus(CorpusSize::Smoke, None);
		let fa = fixtures(&a, CorpusSize::Smoke);
		assert_eq!(corpus_hash(&a, &fa), corpus_hash(&b, &fixtures(&b, CorpusSize::Smoke)));
		assert_eq!(fa.len(), a.len() * SPLINES.len() * 5);
		let full: Vec<String> = corpus(CorpusSize::Full, None).into_iter().map(|s| s.label).collect();
		for family in ["walk/us/offset/irregular", "walk/30d/huge/irregular", "few/1", "constant", "step", "duplicates/shuffled", "burst/smooth", "geometric/alternating", "uci-shape/minute"] {
			assert!(full.iter().any(|l| l == family), "{family} missing from {full:?}");
		}
	}

	#[test]
	fn the_uci_shape_has_minute_cadence_three_decimals_and_gaps() {
		let p = uci_shape(&mut Rng::new(1), 1_440);
		assert_eq!(p.len(), 1_440);
		let gaps: Vec<i64> = p.windows(2).map(|w| (w[1].timestamp - w[0].timestamp).num_minutes()).collect();
		assert!(gaps.iter().all(|&g| g >= 1) && gaps.iter().any(|&g| g > 1), "minute cadence with missing runs");
		assert!(p.iter().all(|x| x.value.fractional_digit_count() <= 3 && to_f64(&x.value) > 0.0));
	}

	#[test]
	fn cpu_and_parallel_keep_the_contract_bit_identically() {
		let options = RunOptions { env: "test".to_owned(), env_vars: BTreeMap::new(), size: CorpusSize::Smoke, repeats: 2, targets: vec![Target::Cpu, Target::Parallel], real: None };
		let (run, values) = run_environment(&options);
		assert_eq!(run.targets.len(), 4);
		for t in &run.targets {
			assert!(t.unavailable.is_none() && t.run_to_run_identical, "{t:?}");
			for m in &t.methods {
				assert!(m.pass, "{} {}: {m:?}", t.key("test"), m.method);
				assert!(m.inside_points > 0 && m.outside_points > 0, "{m:?}");
			}
		}
		let c = corpus(CorpusSize::Smoke, None);
		let f = fixtures(&c, CorpusSize::Smoke);
		let hash = corpus_hash(&c, &f);
		let matrix = build_matrix(&f, &hash, vec![(run, values)], vec![("lavapipe".to_owned(), "not installed".to_owned())]);
		assert!(matrix.failures.is_empty(), "{:?}", matrix.failures);
		// Four targets, six pairs; Cpu and Parallel agree to the bit.
		assert_eq!(matrix.drift.len(), 6);
		let cp = matrix.drift.iter().find(|r| r.a == "test/cpu/decimal" && r.b == "test/parallel/decimal").expect("cpu/parallel pair");
		assert_eq!(cp.drift.identical, cp.drift.points);
		let md = render_markdown(&matrix);
		assert!(md.contains("| test/cpu/decimal | `Linear` |"), "{md}");
		assert!(md.contains("lavapipe | not measured: not installed"));
		assert!(md.contains("Every target that ran kept the published bound"));
	}

	#[test]
	fn the_score_catches_a_violation_and_the_matrix_reports_it() {
		let c = corpus(CorpusSize::Smoke, None);
		let f = fixtures(&c, CorpusSize::Smoke);
		let mut outcomes = run_once(&c, &f, Target::Cpu, Api::F64);
		// Nudge one interpolated value of the first fixture that has one (of a Linear grid) by
		// a millionth of its scale.
		let (fi, i) = outcomes
			.iter()
			.enumerate()
			.find_map(|(fi, o)| match o {
				Outcome::Values { kinds, .. } if f[fi].spline == Spline::Linear => kinds.iter().position(|k| *k == PointKind::Interpolated).map(|i| (fi, i)),
				_ => None,
			})
			.expect("a Linear grid with an interpolated point");
		if let Outcome::Values { bits, .. } = &mut outcomes[fi] {
			bits[i] = (f64::from_bits(bits[i]) + f[fi].scale[i] * 1e-6).to_bits();
		}
		let stats = score(&f, &outcomes, Target::Cpu);
		assert!(!stats[0].pass && stats[0].inside_max > 1e-7, "{:?}", stats[0]);
		assert!(stats[1..].iter().all(|m| m.pass));
		// The same nudge is within the f32 bound.
		assert!(score(&f, &outcomes, Target::GpuF32)[0].pass);
		let clean = values_of(&run_once(&c, &f, Target::Cpu, Api::F64));
		let d = drift(&f, &clean, &values_of(&outcomes));
		assert_eq!(d.points - d.identical, 1);
		assert!((d.max - 1e-6).abs() < 1e-9, "{d:?}");
		let e = drift(&f, &clean, &[vec![None], clean[1..].to_vec()].concat());
		assert_eq!(e.points, d.points - f[0].exact.len() as u64);
		assert_eq!(e.outcome_mismatches, 1);
	}

	#[test]
	fn values_round_trip_through_the_sidecar() {
		let values = vec![vec![Some(vec![0u64, f64::NAN.to_bits(), (-0.0f64).to_bits(), u64::MAX]), None], vec![]];
		let mut buf = Vec::new();
		write_values(&mut buf, &values).expect("writes");
		assert_eq!(read_values(buf.as_slice()).expect("reads"), values);
		assert!(read_values(&b"NOTVALUES"[..]).is_err());
	}

	#[test]
	fn gpu_targets_keep_the_contract_when_a_gpu_is_present() {
		// Starting the GPU is what the target does; without one the targets say why and are
		// not counted as failures.
		let options = RunOptions { env: "gpu".to_owned(), env_vars: BTreeMap::new(), size: CorpusSize::Smoke, repeats: 1, targets: vec![Target::GpuF64, Target::GpuF32], real: None };
		let (run, _) = run_environment(&options);
		for t in &run.targets {
			match &t.unavailable {
				Some(why) => eprintln!("{}: skipped, {why}", t.key("gpu")),
				None => assert!(t.pass(), "{}: {:?}", t.key("gpu"), t.methods),
			}
		}
	}
}
