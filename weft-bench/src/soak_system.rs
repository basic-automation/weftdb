//! Linux system samplers, contamination flags and the strict-mode flush check
//! (1.0 plan SOA-12, W3).
//!
//! A published soak number is a claim about WeftDB running on a machine. Half of
//! what makes it true is the machine: a p99 measured while another tenant was
//! writing 2 GB/s to the same disk, or after the kernel killed a process, or
//! across an NTP step that moved the wall clock the interval log is bucketed by,
//! says nothing about the database. SOA-3 gave the harness self-monitoring — the
//! schedule watching itself — and this module watches the host under it, so a run
//! can be published or refused on evidence rather than on the operator's memory.
//!
//! **Three parts, one artifact.** [`SystemSampler`] reads Linux procfs and turns
//! it into a [`SystemSample`]; [`SystemLedger`] folds a run's samples into
//! [`ContaminationFlag`]s against a [`ContaminationPolicy`]; [`evaluate_flush_check`] answers
//! the one question a durability claim depends on — did the bytes the run was told
//! were durable ever reach the device. Together they serialize as one JSON
//! artifact beside the SOA-1 run record and the SOA-3 scheduler report.
//!
//! **procfs read through an injectable root, not `sysinfo`.** [`ProcRoot`] holds
//! the directory every read is resolved against — `/proc` in production, a fixture
//! directory in a test. That choice is the whole reason the contamination rules are
//! testable: a flag for "the kernel killed something" or "the disk saw 40× the
//! bytes the run wrote" cannot be exercised by asking a live kernel to misbehave,
//! and `sysinfo` (already a dependency, and used by [`crate::report`]'s hardware
//! probe) exposes neither PSI, nor `vmstat`'s `oom_kill`, nor `Dirty`, nor a root to
//! point at a fixture. The cost is a Linux-only module — which is what this slice
//! is; off Linux, [`ProcRoot::system`] still constructs and every read fails, so
//! [`StrictMode::Strict`] fails closed there rather than reporting a clean host it
//! never sampled.
//!
//! **Flags, not thresholds nobody wrote down.** Every threshold that can turn a
//! number into a contamination flag is a named field of [`ContaminationPolicy`]
//! with a default and a comment saying why it is that number, and
//! [`ContaminationPolicy::validate`] refuses an incoherent one. The ledger reports
//! flags with the evidence that raised them ([`ContaminationFlag::evidence`]), so a
//! reviewer can disagree with a threshold without re-running anything. Nothing here
//! grades a *latency*: latency verdicts belong to SOA-13's SLO evaluator, which
//! reads these flags as a precondition.
//!
//! **Fail closed where a durability claim is at stake.** [`evaluate_flush_check`] compares
//! the device's own write counter against the bytes a run was told were sealed. A
//! shortfall is `writes_not_observed` — the honest reading is that the flush was
//! buffered, not durable. A host where the check cannot be evaluated (no procfs, a
//! store on a filesystem with no backing block device, a device absent from
//! `diskstats`) is `skipped` under [`StrictMode::Off`] and **fails** under
//! [`StrictMode::Strict`]: an unevaluated check is not a passed check, and strict
//! mode is exactly the mode in which a run is going to be published.

use std::{
	collections::BTreeMap, path::{Path, PathBuf}, sync::{
		atomic::{AtomicBool, Ordering}, Arc
	}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}
};

use serde::{Deserialize, Serialize};

/// The procfs directory sampled when none is given: the real one.
pub const DEFAULT_PROC_ROOT: &str = "/proc";

/// A sector is 512 bytes in `/proc/diskstats`, whatever the device's physical
/// sector size is; the kernel documents the counters as "in 512-byte sectors".
const DISKSTATS_SECTOR_BYTES: u64 = 512;

/// procfs, read through a root that a test can point somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRoot {
	/// Directory holding `loadavg`, `meminfo`, `diskstats`, ….
	path: PathBuf,
}

impl ProcRoot {
	/// The host's own procfs.
	#[must_use]
	pub fn system() -> Self {
		Self { path: PathBuf::from(DEFAULT_PROC_ROOT) }
	}

	/// A procfs-shaped directory, for tests and for a captured host.
	#[must_use]
	pub fn at(path: impl Into<PathBuf>) -> Self {
		Self { path: path.into() }
	}

	/// The directory this root reads.
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Read one file under the root.
	///
	/// # Errors
	///
	/// Returns an error naming the file when it cannot be read; procfs files fail
	/// for reasons a caller cannot distinguish (missing on an older kernel,
	/// unreadable under ahide, a container that masked it), so the reason is kept.
	fn read(&self, relative: &str) -> anyhow::Result<String> {
		let path = self.path.join(relative);
		std::fs::read_to_string(&path).map_err(|err| anyhow::anyhow!("reading {} failed: {err}", path.display()))
	}

	/// Read one file that older or confined kernels may not have. `None` means
	/// "this host did not report it", which the sample records as such rather than
	/// as a zero.
	fn read_optional(&self, relative: &str) -> Option<String> {
		std::fs::read_to_string(self.path.join(relative)).ok()
	}
}

/// Pressure stall information for cpu, io and memory, in the `avg10` percentages
/// the kernel reports (percentage of the last 10 s the task group was stalled).
///
/// PSI is the contamination signal with the least false positive rate: a load
/// average of 8 on a 64-core box means nothing, while `some avg10=40.00` on io
/// means tasks wanted I/O and could not get it for 40 % of the window. A host with
/// no PSI (`CONFIG_PSI=y` since 4.20) is recorded as [`None`], never as zeros: zeros
/// claim an idle host, `None` admits the host was not observed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PressureStall {
	/// `/proc/pressure/cpu` `some avg10`.
	pub cpu_some_avg10: f64,
	/// `/proc/pressure/io` `some avg10`.
	pub io_some_avg10: f64,
	/// `/proc/pressure/memory` `some avg10`.
	pub memory_some_avg10: f64,
	/// `/proc/pressure/io` `full avg10`, the complete-stall counterpart of
	/// [`PressureStall::io_some_avg10`]: time when *every* task was stalled.
	pub io_full_avg10: f64,
}

impl PressureStall {
	/// Parse the three PSI files of one procfs root.
	///
	/// # Errors
	///
	/// Returns an error when a file exists but does not parse; a missing file is
	/// `Ok(None)`.
	pub fn read(root: &ProcRoot) -> anyhow::Result<Option<Self>> {
		let Some(cpu) = read_pressure(root, "pressure/cpu")? else { return Ok(None) };
		let io = read_pressure(root, "pressure/io")?.unwrap_or_default();
		let memory = read_pressure(root, "pressure/memory")?.unwrap_or_default();
		Ok(Some(Self { cpu_some_avg10: cpu.0, io_some_avg10: io.0, memory_some_avg10: memory.0, io_full_avg10: io.1 }))
	}
}

/// Read `some`/`full` `avg10` from one PSI file.
fn read_pressure(root: &ProcRoot, relative: &str) -> anyhow::Result<Option<(f64, f64)>> {
	let Some(text) = root.read_optional(relative) else { return Ok(None) };
	let mut some = None;
	let mut full = 0.0;
	for line in text.lines() {
		let mut fields = line.split_whitespace();
		match fields.next() {
			Some("some") => some = Some(pressure_avg10(fields.next()).ok_or_else(|| anyhow::anyhow!("{} has no some avg10", root.path.join(relative).display()))?),
			Some("full") => full = pressure_avg10(fields.next()).unwrap_or(0.0),
			_ => {}
		}
	}
	some.map(|some| (some, full)).map(Some).ok_or_else(|| anyhow::anyhow!("{} has no some line", root.path.join(relative).display()))
}

/// Pull `avg10=NN.NN` out of one PSI field.
fn pressure_avg10(field: Option<&str>) -> Option<f64> {
	let rest = field?.strip_prefix("avg10=")?;
	rest.parse().ok()
}

/// One block device's cumulative counters, as `/proc/diskstats` reports them
/// since boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCounters {
	/// Kernel device name the line matched (e.g. `nvme0n1p2`, `dm-0`).
	pub name: String,
	/// Sectors read since boot, in [`DISKSTATS_SECTOR_BYTES`]-byte units.
	pub sectors_read: u64,
	/// Sectors written since boot, in [`DISKSTATS_SECTOR_BYTES`]-byte units.
	pub sectors_written: u64,
	/// Requests currently in flight.
	pub in_progress: u64,
	/// Milliseconds this device had at least one request in flight.
	pub millis_doing_io: u64,
}

impl DeviceCounters {
	/// Bytes written since boot, in bytes.
	#[must_use]
	pub fn bytes_written(&self) -> u64 {
		self.sectors_written.saturating_mul(DISKSTATS_SECTOR_BYTES)
	}

	/// Bytes read since boot, in bytes.
	#[must_use]
	pub fn bytes_read(&self) -> u64 {
		self.sectors_read.saturating_mul(DISKSTATS_SECTOR_BYTES)
	}

	/// Milliseconds per `elapsed_ms` of wall time this device was busy, which is
	/// the utilised fraction the kernel would print in `iostat` as `%util`, in
	/// percent and uncapped: over 100 % means the window was shorter than the
	/// device's own accounting, and it is reported rather than clamped.
	#[must_use]
	pub fn utilisation_percent(&self, previous: &Self, elapsed_ms: u64) -> f64 {
		if elapsed_ms == 0 {
			return 0.0;
		}
		let busy = self.millis_doing_io.saturating_sub(previous.millis_doing_io);
		(busy as f64 / elapsed_ms as f64) * 100.0
	}
}

/// Aggregate CPU jiffies, for a utilization delta between two samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuJiffies {
	/// Jiffies spent idle or waiting for I/O.
	pub idle: u64,
	/// Jiffies of every state, idle included.
	pub total: u64,
}

impl CpuJiffies {
	/// Fraction of CPU time in use between this sample and an earlier one, in
	/// percent. A counter reset (a reboot inside the window, which a soak cannot
	/// survive but a sampler can) yields 0 rather than a wrap-around number.
	#[must_use]
	pub fn utilization_percent_since(&self, previous: &Self) -> f64 {
		let total = self.total.saturating_sub(previous.total);
		if total == 0 {
			return 0.0;
		}
		let idle = self.idle.saturating_sub(previous.idle);
		((total - idle.min(total)) as f64 / total as f64) * 100.0
	}
}

/// One reading of the host.
///
/// Two clocks are recorded on purpose. `monotonic_ms` is the sampler's own clock
/// and is what a duration is measured on; `wall_unix_ms` is the clock the interval
/// log buckets by, and the difference between their deltas across one sample step
/// is how a clock adjustment becomes visible
/// ([`ContaminationKind::ClockStep`]). The sample carries no path, no hostname and
/// no free text: an artifact built from samples needs no redaction pass to be safe
/// to publish, and the fields stay that way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemSample {
	/// Milliseconds since the sampler's own start; strictly increasing.
	pub monotonic_ms: u64,
	/// Wall clock, milliseconds since the Unix epoch.
	pub wall_unix_ms: u64,
	/// 1, 5 and 15 minute load average.
	pub load_avg: [f64; 3],
	/// Online CPU count, from `/proc/stat`'s per-cpu line count (falling back to
	/// the machine's own view when procfs has no per-cpu lines).
	pub cpu_count: usize,
	/// CPU utilization against the previous sample of the same sampler, in
	/// percent; `None` on the first sample.
	pub cpu_utilization_percent: Option<f64>,
	/// `/proc/meminfo` `MemTotal`.
	pub mem_total_kib: u64,
	/// `/proc/meminfo` `MemAvailable` — the honest "how much is left" number.
	pub mem_available_kib: u64,
	/// `/proc/meminfo` `Dirty`: written by processes, not yet by the device.
	pub dirty_kib: u64,
	/// `/proc/meminfo` `Writeback`: currently being written by the device.
	pub writeback_kib: u64,
	/// `/proc/vmstat` `pswpout`, cumulative.
	pub swap_out_pages: u64,
	/// `/proc/vmstat` `pgmajfault`, cumulative.
	pub major_faults: u64,
	/// `/proc/vmstat` `oom_kill`, cumulative. A nonzero delta is never noise.
	pub oom_kills: u64,
	/// PSI, or `None` when this kernel does not report it.
	pub pressure: Option<PressureStall>,
	/// CPU jiffies, or `None` when `/proc/stat` was unreadable.
	pub cpu_jiffies: Option<CpuJiffies>,
	/// The store device's counters, or `None` when no device was resolved.
	pub device: Option<DeviceCounters>,
}

impl SystemSample {
	/// Bytes of page-cache writeback still outstanding, in bytes.
	#[must_use]
	pub fn unwritten_bytes(&self) -> u64 {
		(self.dirty_kib + self.writeback_kib).saturating_mul(1024)
	}
}

/// The block device a store directory sits on, as procfs names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreDevice {
	/// Kernel device name as it appears in `/proc/diskstats` (e.g. `nvme0n1p2`).
	pub name: String,
	/// The mount point that matched, longest prefix wins.
	pub mount_point: String,
	/// How the name was found: `mount_source` from `/proc/self/mountinfo`, or
	/// `major_minor` through `/proc/partitions` when the mount source is not a
	/// `/dev` path (a `tmpfs`, a `overlay`, or a bind mount that lost its source).
	pub source: String,
}

/// Resolve the device a store directory is on.
///
/// `None` means procfs could not attribute the directory to a block device — the
/// honest answer for a `tmpfs` store, and the answer that makes
/// [`evaluate_flush_check`] reporting
/// [`FlushFailureKind::DeviceUnresolved`] rather than passing.
#[must_use]
pub fn resolve_store_device(root: &ProcRoot, store_dir: &Path) -> Option<StoreDevice> {
	let mountinfo = root.read_optional("self/mountinfo")?;
	let mounts = parse_mountinfo(&mountinfo);
	let (_, mount) = longest_mount_prefix(store_dir, &mounts)?;
	// `/proc/partitions` names the `major:minor` that `mountinfo` carries, and it
	// works whatever the mount source looked like - a plain partition, a
	// `/dev/mapper/vg-root` link whose basename is a volume group, or a UUID link.
	// The source basename is the fallback for a kernel that hides the table, and it
	// is only a name: a multi-device filesystem (btrfs across disks) reports one
	// source device, so its counters cover that device alone.
	if let Some(name) = root.read_optional("partitions").and_then(|table| parse_partitions(&table).get(&mount.major_minor).cloned()) {
		return Some(StoreDevice { name, mount_point: mount.mount_point.clone(), source: "major_minor".to_string() });
	}
	let source = mount.source_device()?;
	Some(StoreDevice { name: source.to_string(), mount_point: mount.mount_point.clone(), source: "mount_source".to_string() })
}

/// One `/proc/self/mountinfo` row, narrowed to what device attribution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountRow {
	major_minor: String,
	mount_point: String,
	/// The mount source field, usually `/dev/nvme0n1p2`.
	source: String,
}

impl MountRow {
	/// The `/dev` basename of the mount source, when it is one.
	fn source_device(&self) -> Option<&str> {
		self.source.strip_prefix("/dev/")?.rsplit('/').next().filter(|name| !name.is_empty())
	}
}

/// Parse `/proc/self/mountinfo`, skipping the optional-fields block correctly.
fn parse_mountinfo(text: &str) -> Vec<MountRow> {
	let mut rows = Vec::new();
	for line in text.lines() {
		let Some((head, tail)) = line.split_once(" - ") else { continue };
		let head: Vec<&str> = head.split_whitespace().collect();
		let tail: Vec<&str> = tail.split_whitespace().collect();
		if head.len() < 5 || tail.len() < 2 {
			continue;
		}
		rows.push(MountRow { major_minor: head[2].to_string(), mount_point: unescape_mount_field(head[4]), source: tail[1].to_string() });
	}
	rows
}

/// Turn one `major:minor` line of `/proc/partitions` into `major:minor -> name`.
fn parse_partitions(text: &str) -> BTreeMap<String, String> {
	let mut map = BTreeMap::new();
	for line in text.lines() {
		let fields: Vec<&str> = line.split_whitespace().collect();
		if fields.len() < 4 || !fields[0].chars().all(|c| c.is_ascii_digit()) {
			continue;
		}
		map.insert(format!("{}:{}", fields[0], fields[1]), fields[3].to_string());
	}
	map
}

/// True when `device` appears as a name in `/proc/diskstats`.
fn diskstats_lists(text: &str, device: &str) -> bool {
	text.lines().any(|line| line.split_whitespace().nth(2) == Some(device))
}

/// `/proc/self/mountinfo` escapes space, tab, newline and backslash octally.
fn unescape_mount_field(field: &str) -> String {
	let bytes = field.as_bytes();
	let mut out = String::with_capacity(field.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == b'\\' && index + 4 <= bytes.len() && matches!(bytes[index + 1], b'0'..=b'7') {
			if let Ok(decoded) = u8::from_str_radix(&field[index + 1..index + 4], 8) {
				out.push(char::from(decoded));
				index += 4;
				continue;
			}
		}
		out.push(bytes[index] as char);
		index += 1;
	}
	out
}

impl MountRow {
	/// True when this mount's path is a prefix of `target`.
	///
	/// `/` is a prefix of every absolute path, so it matches rather than being
	/// special-cased away - a store on the root filesystem is on a real device.
	fn matches(&self, target: &str) -> bool {
		if self.mount_point == "/" {
			return target.starts_with('/');
		}
		target == self.mount_point || target.starts_with(&format!("{}/", self.mount_point))
	}
}

/// The mount whose path is the longest prefix of `dir`.
fn longest_mount_prefix<'a>(dir: &Path, mounts: &'a [MountRow]) -> Option<(usize, &'a MountRow)> {
	let target = dir.to_string_lossy();
	mounts.iter().filter(|mount| mount.matches(&target)).map(|mount| (mount.mount_point.len(), mount)).max_by_key(|(length, _)| *length)
}

/// Read one host's procfs into [`SystemSample`]s.
#[derive(Debug, Clone)]
pub struct SystemSampler {
	root: ProcRoot,
	/// Device to sample alongside the host, when one was resolved.
	device: Option<String>,
	/// The instant this sampler started: `monotonic_ms` is measured from it.
	start: Instant,
	/// The previous sample's jiffies, for a utilization delta.
	previous_jiffies: Option<CpuJiffies>,
}

impl SystemSampler {
	/// A sampler for the host's own procfs, with no device until one is resolved.
	#[must_use]
	pub fn linux() -> Self {
		Self::at(ProcRoot::system(), None)
	}

	/// A sampler reading `root`, counting `device` when given.
	#[must_use]
	pub fn at(root: ProcRoot, device: Option<String>) -> Self {
		Self { root, device, start: Instant::now(), previous_jiffies: None }
	}

	/// The procfs this sampler reads.
	#[must_use]
	pub fn root(&self) -> &ProcRoot {
		&self.root
	}

	/// Resolve `store_dir`'s device through this sampler's root and count it from
	/// now on. Returns the resolution so a caller can record or refuse it.
	pub fn track_store_dir(&mut self, store_dir: &Path) -> Option<StoreDevice> {
		let device = resolve_store_device(&self.root, store_dir)?;
		self.device = Some(device.name.clone());
		Some(device)
	}

	/// Take one sample, and remember its jiffies for the next utilization delta.
	///
	/// # Errors
	///
	/// Returns an error when the files every Linux has (`loadavg`, `meminfo`,
	/// `vmstat`) cannot be read; per-kernel extras (PSI, `diskstats`, a single
	/// device's line) degrade to `None` inside the sample instead.
	pub fn sample(&mut self) -> anyhow::Result<SystemSample> {
		let monotonic_ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
		let wall_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
		let load_avg = parse_loadavg(&self.root.read("loadavg")?)?;
		let meminfo = self.root.read("meminfo")?;
		let vmstat = self.root.read("vmstat")?;
		let stat = self.root.read("stat")?;
		let jiffies = parse_cpu_jiffies(&stat);
		let cpu_count = parse_cpu_count(&stat).max(1);
		let cpu_utilization_percent = match (jiffies, self.previous_jiffies) {
			(Some(current), Some(previous)) => Some(current.utilization_percent_since(&previous)),
			_ => None,
		};
		if let Some(jiffies) = jiffies {
			self.previous_jiffies = Some(jiffies);
		}
		Ok(SystemSample { monotonic_ms, wall_unix_ms, load_avg, cpu_count, cpu_utilization_percent, mem_total_kib: meminfo_kib(&meminfo, "MemTotal:"), mem_available_kib: meminfo_kib(&meminfo, "MemAvailable:"), dirty_kib: meminfo_kib(&meminfo, "Dirty:"), writeback_kib: meminfo_kib(&meminfo, "Writeback:"), swap_out_pages: vmstat_value(&vmstat, "pswpout"), major_faults: vmstat_value(&vmstat, "pgmajfault"), oom_kills: vmstat_value(&vmstat, "oom_kill"), pressure: PressureStall::read(&self.root)?, cpu_jiffies: jiffies, device: self.device.as_deref().and_then(|name| parse_diskstats_line(&self.root.read_optional("diskstats").unwrap_or_default(), name)) })
	}

	/// Take one sample without advancing the utilization baseline, for a caller
	/// that wants a starting point to diff against.
	///
	/// # Errors
	///
	/// As [`SystemSampler::sample`].
	pub fn peek(&self) -> anyhow::Result<SystemSample> {
		let mut clone = self.clone();
		clone.sample()
	}
}

/// Parse `/proc/loadavg`: `0.52 0.47 0.40 1/533 29411`.
fn parse_loadavg(text: &str) -> anyhow::Result<[f64; 3]> {
	let mut values = [0.0; 3];
	for (field, slot) in text.split_whitespace().take(3).zip(values.iter_mut()) {
		*slot = field.parse().map_err(|_| anyhow::anyhow!("loadavg has a non-numeric field ({field:?})"))?;
	}
	Ok(values)
}

/// One `Key:  1234 kB` line of `/proc/meminfo`, in KiB; 0 when absent.
fn meminfo_kib(text: &str, key: &str) -> u64 {
	text.lines().find_map(|line| line.split_once(key)).and_then(|(_, rest)| rest.split_whitespace().next().and_then(|value| value.parse().ok())).unwrap_or(0)
}

/// One `key value` line of `/proc/vmstat`, 0 when absent (an older kernel has no
/// `oom_kill` line, which must read as "never observed", not as "zero observed";
/// the distinction is recorded by [`ContaminationPolicy`] treating a constant
/// counter as no delta).
fn vmstat_value(text: &str, key: &str) -> u64 {
	text.lines().filter(|line| line.split_once(' ').is_some_and(|(name, _)| name == key)).find_map(|line| line.split_once(' ')).and_then(|(_, value)| value.parse().ok()).unwrap_or(0)
}

/// The aggregate `cpu ` line of `/proc/stat` as jiffies.
fn parse_cpu_jiffies(text: &str) -> Option<CpuJiffies> {
	let line = text.lines().find(|line| line.starts_with("cpu "))?;
	let fields: Vec<u64> = line.split_whitespace().skip(1).filter_map(|field| field.parse().ok()).collect();
	if fields.len() < 5 {
		return None;
	}
	let idle = fields.get(3).copied().unwrap_or(0).saturating_add(fields.get(4).copied().unwrap_or(0));
	Some(CpuJiffies { idle, total: fields.iter().copied().sum() })
}

/// Count the per-cpu lines of `/proc/stat`.
fn parse_cpu_count(text: &str) -> usize {
	text.lines().filter(|line| line.starts_with("cpu")).count().saturating_sub(1)
}

/// One device's `/proc/diskstats` line.
fn parse_diskstats_line(text: &str, name: &str) -> Option<DeviceCounters> {
	let fields = text.lines().map(|line| line.split_whitespace().collect::<Vec<_>>()).find(|fields| fields.get(2).is_some_and(|field| *field == name))?;
	// Fields per kernel docs: 0 major, 1 minor, 2 name, 3 reads, 4 reads merged,
	// 5 sectors read, 6 ms reading, 7 writes, 8 writes merged, 9 sectors written,
	// 10 ms writing, 11 in flight, 12 ms doing I/O.
	if fields.len() < 13 {
		return None;
	}
	Some(DeviceCounters { name: name.to_string(), sectors_read: fields[5].parse().ok()?, sectors_written: fields[9].parse().ok()?, in_progress: fields[11].parse().ok()?, millis_doing_io: fields[12].parse().ok()? })
}

/// The thresholds a run is judged against, as numbers in the artifact rather than
/// as an operator's judgement call.
///
/// Two presets ship:
///
/// * [`ContaminationPolicy::default`] is the **screening** policy: it fires on
///   conditions that make a latency or throughput number meaningless rather than
///   on anything that merely moved - `Dirty` at 256 MiB, 64 pages/s of swap-out, a
///   10 percent-per-second IO stall figure. It is what a scheduled nightly uses so
///   that a busy shared machine does not fail every run.
/// * [`ContaminationPolicy::strict`] is the **publication** policy, and is what the
///   release-gate job (SOA-7) is expected to use: any IO or CPU stall above 1
///   percent, no swap-out at all, and no foreign write above 1 MiB.
///
/// Every field is a limit *inclusive*: a sample exactly at the limit is clean.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContaminationPolicy {
	/// Highest tolerated `some avg10` IO stall, in percent of the window.
	pub max_io_psi_some_avg10: f64,
	/// Highest tolerated `some avg10` CPU stall.
	pub max_cpu_psi_some_avg10: f64,
	/// Highest tolerated `some avg10` memory stall.
	pub max_memory_psi_some_avg10: f64,
	/// Highest tolerated `Dirty` - writeback not keeping up is the classic
	/// end-of-run latency cliff.
	pub max_dirty_kib: u64,
	/// Highest tolerated `/proc/vmstat pswpout` growth per second of window.
	pub max_swap_out_pages_per_sec: f64,
	/// Highest tolerated `/proc/vmstat pgmajfault` growth per second of window.
	pub max_major_faults_per_sec: f64,
	/// Highest tolerated |wall minus monotonic| step between two samples, in
	/// milliseconds; a clock step invalidates every interval measured across it.
	pub max_clock_step_ms: i64,
	/// Longest tolerated gap between two samples, in milliseconds: a sampler that
	/// was starved is itself a contamination signal, and a hole in the record is
	/// not evidence of a quiet machine.
	pub max_sample_gap_ms: u64,
	/// Highest tolerated `load1` per logical CPU - the one signal that still means
	/// something on a kernel without PSI.
	pub max_load_per_core: f64,
	/// Bytes the host may have written through the measured device that this run
	/// cannot account for before `foreign_writer` fires.
	pub foreign_write_tolerance_bytes: u64,
}

impl Default for ContaminationPolicy {
	fn default() -> Self {
		Self { max_io_psi_some_avg10: 10.0, max_cpu_psi_some_avg10: 10.0, max_memory_psi_some_avg10: 10.0, max_dirty_kib: 262_144, max_swap_out_pages_per_sec: 64.0, max_major_faults_per_sec: 256.0, max_clock_step_ms: 250, max_sample_gap_ms: 30_000, max_load_per_core: 4.0, foreign_write_tolerance_bytes: 64 * 1024 * 1024 }
	}
}

impl ContaminationPolicy {
	/// The publication policy: a run is clean only on a machine that was
	/// demonstrably doing nothing else.
	#[must_use]
	pub fn strict() -> Self {
		Self { max_io_psi_some_avg10: 1.0, max_cpu_psi_some_avg10: 1.0, max_memory_psi_some_avg10: 0.0, max_dirty_kib: 8_192, max_swap_out_pages_per_sec: 0.0, max_major_faults_per_sec: 1.0, max_clock_step_ms: 25, max_sample_gap_ms: 5_000, max_load_per_core: 1.5, foreign_write_tolerance_bytes: 1_048_576 }
	}
}

/// Why a run is not clean. Serde uses these exact kebab-case names, and the
/// artifact's `contamination` object is a list of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContaminationFlag {
	/// The measured device was stalling (`some avg10` over policy).
	IoPressure,
	/// The host was CPU-stalled.
	CpuPressure,
	/// The host was memory-stalled.
	MemoryPressure,
	/// `Dirty` exceeded policy, so the run's tail wrote into a backlog.
	DirtyBacklog,
	/// Anonymous pages were swapped out during the window.
	SwapActivity,
	/// Major fault rate exceeded policy - the working set was not resident.
	MajorFaults,
	/// The realtime clock moved relative to the monotonic clock.
	ClockStep,
	/// The sampler itself lost more than `max_sample_gap_ms`.
	SampleGap,
	/// `load1` per core exceeded policy (the PSI-free fallback).
	LoadAverage,
	/// The device wrote more than the run acknowledged, by more than tolerance.
	ForeignWriter,
	/// `/proc/vmstat oom_kill` advanced during the window: a process on this host
	/// was killed for memory, which is contamination whoever it was.
	OomKill,
	/// No sample of the measured device, or no PSI on a kernel the policy expects
	/// it on: the record cannot clear the run either way, so it does not.
	SamplerUnavailable,
}

impl ContaminationFlag {
	/// The stable name used in the artifact.
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::IoPressure => "io_pressure",
			Self::CpuPressure => "cpu_pressure",
			Self::MemoryPressure => "memory_pressure",
			Self::DirtyBacklog => "dirty_backlog",
			Self::SwapActivity => "swap_activity",
			Self::MajorFaults => "major_faults",
			Self::ClockStep => "clock_step",
			Self::SampleGap => "sample_gap",
			Self::LoadAverage => "load_average",
			Self::ForeignWriter => "foreign_writer",
			Self::OomKill => "oom_kill",
			Self::SamplerUnavailable => "sampler_unavailable",
		}
	}
}

/// The worst value seen for each per-sample signal, kept so that a flagged run can
/// be argued about from numbers rather than from a boolean.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct WorstValues {
	/// Highest IO `some avg10`, in percent.
	pub io_psi_some_avg10: f64,
	/// Highest CPU `some avg10`, in percent.
	pub cpu_psi_some_avg10: f64,
	/// Highest memory `some avg10`, in percent.
	pub memory_psi_some_avg10: f64,
	/// Highest `Dirty`, in KiB.
	pub dirty_kib: u64,
	/// Highest `load1` per logical CPU.
	pub load_per_core: f64,
	/// Largest |wall minus monotonic| step, in milliseconds.
	pub clock_step_ms: i64,
	/// Longest gap between samples, in milliseconds.
	pub sample_gap_ms: u64,
}

/// What the samplers concluded about one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContaminationVerdict {
	/// The flags that fired, sorted by name; empty means clean.
	pub flags: Vec<ContaminationFlag>,
	/// Samples recorded in the window.
	pub sample_count: u64,
	/// Window length from first to last sample, in milliseconds.
	pub window_ms: u64,
	/// The peak of each signal, flagged or not.
	pub worst: WorstValues,
	/// Bytes the measured device wrote during the window, when it could be read.
	pub device_written_bytes: Option<u64>,
	/// Bytes the run acknowledged, when the run reported them.
	pub acknowledged_bytes: Option<u64>,
	/// `device_written_bytes` minus `acknowledged_bytes`, saturating: the foreign
	/// write the `foreign_writer` flag is judged on.
	pub foreign_write_bytes: Option<u64>,
	/// Whether `/proc/pressure` existed at all.
	pub psi_capable: bool,
	/// The measured device, by name.
	pub device: Option<String>,
}

impl ContaminationVerdict {
	/// True when nothing fired.
	#[must_use]
	pub fn clean(&self) -> bool {
		self.flags.is_empty()
	}

	/// The `contamination` object of the soak artifact: flags, worst values, and the
	/// byte comparison behind a `foreign_writer` flag. No path and no free text.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails, which it does not for these types.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// The accumulator a sampler thread is recorded into.
#[derive(Debug, Clone)]
pub struct ContaminationLedger {
	policy: ContaminationPolicy,
	device: Option<StoreDevice>,
	psi_capable: bool,
	first: Option<SystemSample>,
	prev: Option<SystemSample>,
	last: Option<SystemSample>,
	window_ms: u64,
	samples: u64,
	flags: std::collections::BTreeSet<ContaminationFlag>,
	worst: WorstValues,
	acknowledged_bytes: Option<u64>,
}

/// The realtime clock expressed on the monotonic timebase, in milliseconds. The two agree
/// to within scheduling jitter while nothing adjusts the wall clock, so a step in this
/// offset between two samples is a step in the wall clock - `adjtimex`, an NTP step, or a
/// VM resume - and every interval measured across it is wrong.
fn clock_offset_ms(sample: &SystemSample) -> i64 {
	i64::try_from(sample.wall_unix_ms).unwrap_or(i64::MAX) - i64::try_from(sample.monotonic_ms).unwrap_or(i64::MAX)
}

/// `u128` to `f64` for the rate math, where the magnitude matters and the last bit
/// does not.
fn u128_to_f64(value: u128) -> f64 {
	value as f64
}

impl ContaminationLedger {
	/// A ledger for a window whose measured device is `device` (resolved by
	/// [`resolve_store_device`]).
	#[must_use]
	pub fn new(policy: ContaminationPolicy, device: Option<StoreDevice>) -> Self {
		Self { policy, device, psi_capable: false, first: None, prev: None, last: None, window_ms: 0, samples: 0, flags: std::collections::BTreeSet::new(), worst: WorstValues::default(), acknowledged_bytes: None }
	}

	/// The policy in force.
	#[must_use]
	pub const fn policy(&self) -> ContaminationPolicy {
		self.policy
	}

	/// The bytes the run acknowledged, for the foreign-write comparison at
	/// [`ContaminationLedger::close`].
	pub fn set_acknowledged_bytes(&mut self, acknowledged_bytes: u64) {
		self.acknowledged_bytes = Some(acknowledged_bytes);
	}

	/// Record one sample and evaluate the per-sample thresholds against it.
	///
	/// A sample the sampler could not read is not fed in: report that through
	/// [`ContaminationLedger::note_sampler_unavailable`] instead, so that the record
	/// says which it was.
	pub fn record(&mut self, sample: &SystemSample) {
		self.samples += 1;
		if self.first.is_none() {
			self.first = Some(sample.clone());
		}
		if let Some(prev) = &self.prev {
			let step_ms = (clock_offset_ms(sample) - clock_offset_ms(prev)).abs();
			if step_ms > self.worst.clock_step_ms {
				self.worst.clock_step_ms = step_ms;
			}
			if step_ms > self.policy.max_clock_step_ms {
				self.flags.insert(ContaminationFlag::ClockStep);
			}
			let gap = sample.monotonic_ms.saturating_sub(prev.monotonic_ms);
			if gap > self.worst.sample_gap_ms {
				self.worst.sample_gap_ms = gap;
			}
			if gap > self.policy.max_sample_gap_ms {
				self.flags.insert(ContaminationFlag::SampleGap);
			}
		}
		if let Some(pressure) = &sample.pressure {
			self.psi_capable = true;
			self.worst.io_psi_some_avg10 = self.worst.io_psi_some_avg10.max(pressure.io_some_avg10);
			self.worst.cpu_psi_some_avg10 = self.worst.cpu_psi_some_avg10.max(pressure.cpu_some_avg10);
			self.worst.memory_psi_some_avg10 = self.worst.memory_psi_some_avg10.max(pressure.memory_some_avg10);
			if pressure.io_some_avg10 > self.policy.max_io_psi_some_avg10 {
				self.flags.insert(ContaminationFlag::IoPressure);
			}
			if pressure.cpu_some_avg10 > self.policy.max_cpu_psi_some_avg10 {
				self.flags.insert(ContaminationFlag::CpuPressure);
			}
			if pressure.memory_some_avg10 > self.policy.max_memory_psi_some_avg10 {
				self.flags.insert(ContaminationFlag::MemoryPressure);
			}
		}
		self.worst.dirty_kib = self.worst.dirty_kib.max(sample.dirty_kib);
		if sample.dirty_kib > self.policy.max_dirty_kib {
			self.flags.insert(ContaminationFlag::DirtyBacklog);
		}
		let per_core = sample.load_avg[0] / sample.cpu_count.max(1) as f64;
		self.worst.load_per_core = self.worst.load_per_core.max(per_core);
		if per_core > self.policy.max_load_per_core {
			self.flags.insert(ContaminationFlag::LoadAverage);
		}
		self.prev = Some(sample.clone());
		self.last = Some(sample.clone());
	}

	/// Note that a sample could not be taken (`sample_count` is not incremented):
	/// the window cannot be cleared, so it is flagged rather than left blank.
	pub fn note_sampler_unavailable(&mut self) {
		self.flags.insert(ContaminationFlag::SamplerUnavailable);
	}

	/// The flags decided so far, before the window closes.
	#[must_use]
	pub fn flags(&self) -> Vec<ContaminationFlag> {
		self.flags.iter().copied().collect()
	}

	/// Close the window: evaluate the rate-based and byte-based thresholds, which
	/// need a first and a last sample, and return the verdict.
	///
	/// Idempotent: calling it twice returns the same verdict, and the second call
	/// does not re-close over a delta that has already been taken.
	pub fn close(&mut self) -> ContaminationVerdict {
		if let (Some(first), Some(last)) = (&self.first, &self.last) {
			let window_ms = last.monotonic_ms.saturating_sub(first.monotonic_ms).max(1);
			self.window_ms = window_ms;
			let seconds = u128_to_f64(u128::from(window_ms)) / 1000.0;
			let swap_out = last.swap_out_pages.saturating_sub(first.swap_out_pages);
			if swap_out != 0 && swap_out as f64 / seconds > self.policy.max_swap_out_pages_per_sec {
				self.flags.insert(ContaminationFlag::SwapActivity);
			}
			let major = last.major_faults.saturating_sub(first.major_faults);
			if major != 0 && major as f64 / seconds > self.policy.max_major_faults_per_sec {
				self.flags.insert(ContaminationFlag::MajorFaults);
			}
			if last.oom_kills > first.oom_kills {
				self.flags.insert(ContaminationFlag::OomKill);
			}
			// The counters are cumulative since boot, so the window's writes are the
			// difference between the last and the first snapshot. The since-boot
			// total would credit this run with every byte the machine ever wrote,
			// which reads as a giant foreign writer on any long-lived host.
			let device_written = self.device.as_ref().and_then(|resolved| {
				let at = |sample: &SystemSample| sample.device.as_ref().filter(|counters| counters.name == resolved.name).map(DeviceCounters::bytes_written);
				Some(at(last)?.checked_sub(at(first)?)?)
			});
			let foreign_write_bytes = device_written.zip(self.acknowledged_bytes).map(|(written, acked)| written.saturating_sub(acked));
			if let Some(foreign) = foreign_write_bytes {
				if foreign > self.policy.foreign_write_tolerance_bytes {
					self.flags.insert(ContaminationFlag::ForeignWriter);
				}
			}
			self.verdict(device_written, foreign_write_bytes)
		} else {
			self.flags.insert(ContaminationFlag::SamplerUnavailable);
			self.verdict(None, None)
		}
	}

	fn verdict(&self, device_written_bytes: Option<u64>, foreign_write_bytes: Option<u64>) -> ContaminationVerdict {
		ContaminationVerdict { flags: self.flags(), sample_count: self.samples, window_ms: self.window_ms, worst: self.worst, device_written_bytes, acknowledged_bytes: self.acknowledged_bytes, foreign_write_bytes, psi_capable: self.psi_capable, device: self.device.as_ref().map(|d| d.name.clone()) }
	}
}

/// Whether the harness is screening a run or publishing a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StrictMode {
	/// Report the check, do not act on it: what a scheduled exploratory soak uses.
	Off,
	/// Act on it: a run the check does not clear is not a result.
	Strict,
}

impl StrictMode {
	/// Parse the CLI value (`off` / `strict`).
	///
	/// # Errors
	///
	/// Returns an error naming the accepted values.
	pub fn parse(value: &str) -> anyhow::Result<Self> {
		match value {
			"off" => Ok(Self::Off),
			"strict" => Ok(Self::Strict),
			other => Err(anyhow::anyhow!("--strict must be `off` or `strict`, found `{other}`")),
		}
	}
}

/// Why the flush check could not clear a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlushFailureKind {
	/// The device behind the store directory could not be resolved from mountinfo:
	/// the check has nothing to read, so it says so instead of passing.
	DeviceUnresolved,
	/// The device resolved but `/proc/diskstats` did not report it for the window.
	SamplerUnavailable,
	/// The device wrote less than the run acknowledged, by more than the slack.
	WritesNotObserved,
	/// The run ended with a writeback backlog larger than the policy allows, so the
	/// acknowledged bytes are a promise the device has not kept.
	DirtyBacklog,
}

impl FlushFailureKind {
	/// The stable name used in the artifact.
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::DeviceUnresolved => "device_unresolved",
			Self::SamplerUnavailable => "sampler_unavailable",
			Self::WritesNotObserved => "writes_not_observed",
			Self::DirtyBacklog => "dirty_backlog",
		}
	}
}

/// The outcome of the flush check: passed, skipped with a reason, or failed with a
/// kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlushCheckResult {
	/// Always `flush_observed`, so the result can be appended to a list of
	/// preflight checks by name.
	pub name: &'static str,
	/// The device the counters came from, so an artifact says what was measured
	/// and not only that something was. `None` is the `device_unresolved` case.
	pub device: Option<String>,
	/// `true` when the device accounted for what the run acknowledged.
	pub passed: bool,
	/// The failure kind, when there was one.
	pub failure: Option<FlushFailureKind>,
	/// Why the check could not run, when it could not: `device_unresolved` or
	/// `sampler_unavailable`. It is set together with `passed: false`, and
	/// [`FlushCheckResult::blocks`] is what decides whether it stops the run.
	pub skipped: Option<&'static str>,
	/// Bytes the measured device wrote during the window.
	pub device_written_bytes: Option<u64>,
	/// Bytes the run was acknowledged.
	pub acknowledged_bytes: u64,
	/// `Dirty` at close, in KiB.
	pub dirty_kib_at_close: u64,
	/// The slack the comparison was made with.
	pub slack_bytes: u64,
}

impl FlushCheckResult {
	/// Whether this result should stop a run in the given mode.
	///
	/// Strict fails closed: a check that could not run - device unresolved, counters
	/// unreadable - blocks the run exactly as a failed one does, because "unverified"
	/// is not evidence that the flush happened.
	#[must_use]
	pub fn blocks(&self, mode: StrictMode) -> bool {
		mode == StrictMode::Strict && !self.passed
	}

	/// The artifact object for this check.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails, which it does not for these types.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

/// What the flush check is given to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushEvidence {
	/// The resolved device name, when it resolved.
	pub device: Option<String>,
	/// Bytes the device wrote across the window, when `/proc/diskstats` reported it.
	pub device_written_bytes: Option<u64>,
	/// Bytes the run was acknowledged for: the ILP the client sent, or the byte
	/// count the store reported as committed, depending on which the run measured.
	pub acknowledged_bytes: u64,
	/// `Dirty` at the closing sample, in KiB.
	pub dirty_kib_at_close: u64,
}

impl FlushEvidence {
	/// Build the evidence from a closed window's verdict plus the closing sample's
	/// `Dirty`.
	#[must_use]
	pub fn from_verdict(verdict: &ContaminationVerdict, dirty_kib_at_close: u64) -> Self {
		Self { device: verdict.device.clone(), device_written_bytes: verdict.device_written_bytes, acknowledged_bytes: verdict.acknowledged_bytes.unwrap_or_default(), dirty_kib_at_close }
	}
}

/// Run the flush check.
///
/// The evaluation does not take the [`StrictMode`]: it reports what the counters
/// said, and [`FlushCheckResult::blocks`] is where strictness is applied - so an
/// advisory run and a strict run over the same window carry the *same* numbers, and
/// cannot disagree about the facts while disagreeing about the verdict.
///
/// The comparison is `device_written + dirty_bytes + slack >= acknowledged`, where
/// `dirty_bytes` is the host's unwritten backlog at close. The reasoning is that the
/// bytes a run acknowledged are either on the measured device or still in the page
/// cache; anything else means the write went somewhere the sampler is not watching
/// (a network filesystem, a dm-crypt mapping that resolves to a different counter, a
/// store directory that is not the one the server writes to).
///
/// `slack_bytes` covers what the kernel is allowed to have absorbed without it being
/// a problem - a store that compresses writes less than it acknowledged, and the
/// journaling overhead that makes the converse true. Pass 0 to make the comparison
/// exact. It is a parameter rather than a constant so the caller - which knows
/// whether it measured acknowledged bytes at the client or at the store - decides.
#[must_use]
pub fn evaluate_flush_check(evidence: &FlushEvidence, slack_bytes: u64) -> FlushCheckResult {
	let dirty_bytes = evidence.dirty_kib_at_close.saturating_mul(1024);
	let accounted = evidence.device_written_bytes.unwrap_or(0).saturating_add(dirty_bytes).saturating_add(slack_bytes);
	let (passed, failure, skipped) = if evidence.device.is_none() {
		(false, Some(FlushFailureKind::DeviceUnresolved), Some("device_unresolved"))
	} else if evidence.device_written_bytes.is_none() {
		(false, Some(FlushFailureKind::SamplerUnavailable), Some("sampler_unavailable"))
	} else if evidence.acknowledged_bytes > accounted {
		(false, Some(FlushFailureKind::WritesNotObserved), None)
	} else if evidence.dirty_kib_at_close.saturating_mul(1024) > DIRTY_FLUSH_LIMIT_BYTES {
		// A run that ends with a dirty backlog bigger than the host can drain has
		// not flushed in any sense a restart would confirm, whatever the counters
		// say - the acknowledged bytes are still a promise.
		(false, Some(FlushFailureKind::DirtyBacklog), None)
	} else {
		(true, None, None)
	};
	FlushCheckResult { name: "flush_observed", device: evidence.device.clone(), passed, failure: if passed { None } else { failure }, skipped: if passed { None } else { skipped }, device_written_bytes: evidence.device_written_bytes, acknowledged_bytes: evidence.acknowledged_bytes, dirty_kib_at_close: evidence.dirty_kib_at_close, slack_bytes }
}

/// The largest `Dirty`, in bytes, a run may leave behind and still be called
/// flushed: 256 MiB, the same figure [`ContaminationPolicy::default`] screens on.
const DIRTY_FLUSH_LIMIT_BYTES: u64 = 262_144 * 1024;

/// The samples taken by [`SamplerThread::start`], with the count of reads that
/// failed.
#[derive(Debug, Clone)]
pub struct SamplerShutdown {
	/// The samples, oldest first. Empty when the sampler never succeeded.
	pub samples: Vec<SystemSample>,
	/// Reads that returned an error, so a run whose sampler died is distinguishable
	/// from a run that only started late.
	pub failures: u64,
}

/// A sampler on its own thread, deliberately not on the async runtime.
///
/// A sampler that shares a worker thread with the load it is measuring is a sampler
/// that stops when the load saturates the pool - exactly when its numbers matter - so
/// this is a plain OS thread at `SCHED_OTHER` priority, woken in short slices so
/// [`SamplerThread::stop`] does not wait out a whole interval.
#[derive(Debug)]
pub struct SamplerThread {
	stop: Arc<AtomicBool>,
	handle: Option<std::thread::JoinHandle<SamplerShutdown>>,
}

impl SamplerThread {
	/// Start sampling every `every`, moving `sampler` onto the thread.
	///
	/// # Errors
	///
	/// Returns an error when the thread cannot be spawned.
	pub fn start(mut sampler: SystemSampler, every: std::time::Duration) -> anyhow::Result<Self> {
		let stop = Arc::new(AtomicBool::new(false));
		let thread_stop = Arc::clone(&stop);
		let handle = std::thread::Builder::new().name("weft-soak-sampler".to_string()).spawn(move || {
			let mut samples = Vec::new();
			let mut failures = 0_u64;
			while !thread_stop.load(Ordering::Relaxed) {
				match sampler.sample() {
					Ok(sample) => samples.push(sample),
					Err(_) => failures += 1,
				}
				let mut waited = Duration::ZERO;
				while waited < every && !thread_stop.load(Ordering::Relaxed) {
					let slice = (every - waited).min(Duration::from_millis(10));
					std::thread::sleep(slice);
					waited += slice;
				}
			}
			SamplerShutdown { samples, failures }
		})?;
		Ok(Self { stop, handle: Some(handle) })
	}

	/// Stop the thread and take the samples.
	///
	/// # Errors
	///
	/// Returns an error when the sampler thread panicked; its samples are lost with
	/// it, which is why this does not pretend to return them.
	pub fn stop(mut self) -> anyhow::Result<SamplerShutdown> {
		self.stop.store(true, Ordering::Relaxed);
		let handle = self.handle.take().ok_or_else(|| anyhow::anyhow!("sampler thread already stopped"))?;
		Ok(handle.join().map_err(|_| anyhow::anyhow!("sampler thread panicked"))?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A procfs fixture per test name, cleaned before and after, so the sampler is
	/// tested against known counter values instead of against whatever the host
	/// happens to be doing.
	struct Fixture {
		root: PathBuf,
	}

	impl Fixture {
		fn new(name: &str) -> Self {
			let root = std::env::temp_dir().join(format!("weftdb-soa-12-{}-{}", name, std::process::id()));
			let _ = std::fs::remove_dir_all(&root);
			std::fs::create_dir_all(&root).expect("fixture dir");
			Self { root }
		}

		fn write(&self, relative: &str, text: &str) {
			let path = self.root.join(relative);
			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent).expect("fixture parent");
			}
			std::fs::write(path, text).expect("fixture write");
		}

		fn root(&self) -> ProcRoot {
			ProcRoot::at(&self.root)
		}
	}

	impl Drop for Fixture {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.root);
		}
	}

	fn host_fixture(fixture: &Fixture) {
		fixture.write("loadavg", "0.52 0.47 0.40 1/533 29411\n");
		fixture.write("meminfo", "MemTotal:       128000000 kB\nMemFree:         1000000 kB\nMemAvailable:   40000000 kB\nBuffers:          100000 kB\nDirty:              1024 kB\nWriteback:             0 kB\nSwapTotal:      8000000 kB\nSwapFree:       7000000 kB\n");
		fixture.write("vmstat", "nr_free_pages 1000\npswpout 5000\npgmajfault 20\noom_kill 0\n");
		fixture.write("stat", "cpu  100 20 30 800 40 0 5 0 0 0\ncpu0 50 10 15 400 20 0 2 0 0 0\ncpu1 50 10 15 400 20 0 3 0 0 0\n");
		fixture.write("diskstats", " 259   0 nvme0n1 1000 0 2000 300 5000 0 60000 900 0 400 1200 0 0 0 0 0 0\n 259   1 nvme0n1p1 10 0 20 1 2 0 4 1 0 8 9 0 0 0 0 0 0\n 259   2 nvme0n1p2 990 0 1980 299 4998 0 59996 899 0 392 1191 0 0 0 0 0 0\n");
	}

	#[test]
	fn missing_files_are_none_not_zero() {
		let fixture = Fixture::new("missing");
		assert!(ProcRoot::at(&fixture.root).read_optional("pressure/io").is_none());
		assert!(PressureStall::read(&fixture.root()).expect("read").is_none());
		assert!(parse_diskstats_line("", "nvme0n1").is_none());
	}

	#[test]
	fn required_files_missing_is_an_error() {
		let fixture = Fixture::new("required");
		let mut sampler = SystemSampler::at(fixture.root(), None);
		let error = sampler.sample().expect_err("no loadavg on an empty root");
		assert!(error.to_string().contains("loadavg"), "{error}");
	}

	#[test]
	fn pressure_parses_some_and_full() {
		let fixture = Fixture::new("pressure");
		fixture.write("pressure/cpu", "some avg10=1.20 avg60=2.00 avg300=3.00 total=4000\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
		fixture.write("pressure/io", "some avg10=7.50 avg60=8.00 avg300=9.00 total=5000\nfull avg10=3.25 avg60=4.00 avg300=5.00 total=6000\n");
		fixture.write("pressure/memory", "some avg10=0.10 avg60=0.20 avg300=0.30 total=700\nfull avg10=0.05 avg60=0.10 avg300=0.15 total=350\n");
		let pressure = PressureStall::read(&fixture.root()).expect("read").expect("present");
		assert_eq!(pressure, PressureStall { cpu_some_avg10: 1.2, io_some_avg10: 7.5, memory_some_avg10: 0.1, io_full_avg10: 3.25 });
	}

	#[test]
	fn malformed_pressure_is_an_error() {
		let fixture = Fixture::new("pressure-bad");
		fixture.write("pressure/cpu", "some avg60=2.00 total=4000\n");
		let error = PressureStall::read(&fixture.root()).expect_err("no avg10");
		assert!(error.to_string().contains("pressure/cpu"), "{error}");
	}

	#[test]
	fn sample_reads_the_fixture_host() {
		let fixture = Fixture::new("sample");
		host_fixture(&fixture);
		fixture.write("pressure/cpu", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
		fixture.write("pressure/io", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
		fixture.write("pressure/memory", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
		let mut sampler = SystemSampler::at(fixture.root(), Some("nvme0n1p2".to_string()));
		let first = sampler.sample().expect("first sample");
		assert_eq!(first.load_avg, [0.52, 0.47, 0.40]);
		assert_eq!(first.cpu_count, 2);
		assert_eq!(first.mem_total_kib, 128_000_000);
		assert_eq!(first.dirty_kib, 1024);
		assert_eq!(first.swap_out_pages, 5000);
		assert_eq!(first.major_faults, 20);
		assert_eq!(first.oom_kills, 0);
		assert!(first.pressure.is_some(), "PSI present in the fixture");
		assert!(first.cpu_utilization_percent.is_none(), "the first sample has no baseline");
		let device = first.device.expect("device counters");
		assert_eq!(device.bytes_written(), 59_996 * DISKSTATS_SECTOR_BYTES);
		assert_eq!(device.bytes_read(), 1_980 * DISKSTATS_SECTOR_BYTES);
		let second = sampler.sample().expect("second sample");
		assert!(second.cpu_utilization_percent.is_some(), "the second sample has a baseline");
		assert!(second.monotonic_ms >= first.monotonic_ms);
	}

	#[test]
	fn unknown_device_records_none() {
		let fixture = Fixture::new("device-absent");
		host_fixture(&fixture);
		let mut sampler = SystemSampler::at(fixture.root(), Some("nvme1n1".to_string()));
		assert!(sampler.sample().expect("sample").device.is_none());
	}

	#[test]
	fn device_resolves_from_the_mount_source() {
		let fixture = Fixture::new("mount-source");
		fixture.write("self/mountinfo", "36 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n42 36 259:1 /boot /boot rw,relatime shared:2 - ext4 /dev/nvme0n1p1 rw\n");
		let device = resolve_store_device(&fixture.root(), Path::new("/var/lib/weftdb/store")).expect("resolve");
		assert_eq!(device.name, "nvme0n1p2");
		assert_eq!(device.mount_point, "/");
		assert_eq!(device.source, "mount_source");
	}

	#[test]
	fn device_falls_back_to_partitions_by_id() {
		let fixture = Fixture::new("mount-id");
		fixture.write("self/mountinfo", "36 1 253:0 / / rw,relatime - ext4 /dev/mapper/vg-root rw\n50 36 8:1 /mnt /mnt rw - ext4 /dev/sda1 rw\n");
		fixture.write("partitions", "major minor  #blocks  name\n 253        0   99999999 dm-0\n   8        1   49999999 sda1\n");
		let device = resolve_store_device(&fixture.root(), Path::new("/mnt/data")).expect("resolve");
		assert_eq!(device.name, "sda1");
		let mapped = resolve_store_device(&fixture.root(), Path::new("/")).expect("resolve mapped");
		assert_eq!(mapped.name, "dm-0");
		assert_eq!(mapped.source, "major_minor");
	}

	#[test]
	fn tmpfs_and_unknown_directories_do_not_resolve() {
		let fixture = Fixture::new("mount-none");
		fixture.write("self/mountinfo", "28 0 0:25 / / rw,relatime - tmpfs tmpfs rw\n");
		assert!(resolve_store_device(&fixture.root(), Path::new("/tmp/store")).is_none());
		assert!(resolve_store_device(&ProcRoot::at(std::env::temp_dir()), Path::new("/nope")).is_none(), "a root without mountinfo resolves nothing");
	}

	#[test]
	fn escaped_mount_points_match() {
		let fixture = Fixture::new("mount-escape");
		fixture.write("self/mountinfo", "36 1 259:3 / /mnt/my\\040data rw - ext4 /dev/nvme0n1p3 rw\n");
		let device = resolve_store_device(&fixture.root(), Path::new("/mnt/my data/store")).expect("resolve");
		assert_eq!(device.name, "nvme0n1p3");
	}

	fn sample_at(monotonic_ms: u64, wall_unix_ms: u64) -> SystemSample {
		SystemSample { monotonic_ms, wall_unix_ms, load_avg: [0.5, 0.5, 0.5], cpu_count: 8, cpu_utilization_percent: Some(10.0), mem_total_kib: 1_000_000, mem_available_kib: 500_000, dirty_kib: 100, writeback_kib: 0, swap_out_pages: 0, major_faults: 0, oom_kills: 0, pressure: Some(PressureStall { cpu_some_avg10: 0.0, io_some_avg10: 0.0, memory_some_avg10: 0.0, io_full_avg10: 0.0 }), cpu_jiffies: None, device: None }
	}

	fn device_sample_at(monotonic_ms: u64, written_sectors: u64) -> SystemSample {
		let mut sample = sample_at(monotonic_ms, 1_700_000_000_000 + monotonic_ms);
		sample.device = Some(DeviceCounters { name: "nvme0n1p2".to_string(), sectors_read: 0, sectors_written: written_sectors, in_progress: 0, millis_doing_io: 0 });
		sample
	}

	#[test]
	fn a_quiet_window_is_clean() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::strict(), None);
		for offset in [0_u64, 1000, 2000, 3000] {
			ledger.record(&sample_at(offset, 1_700_000_000_000 + offset));
		}
		let verdict = ledger.close();
		assert!(verdict.clean(), "{:?}", verdict.flags);
		assert_eq!(verdict.sample_count, 4);
		assert_eq!(verdict.window_ms, 3000);
		assert!(verdict.psi_capable);
		assert_eq!(serde_json::to_string(&verdict.flags).expect("json"), "[]");
	}

	#[test]
	fn io_pressure_over_policy_flags_the_window() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), None);
		let mut hot = sample_at(0, 1_700_000_000_000);
		hot.pressure = Some(PressureStall { cpu_some_avg10: 0.0, io_some_avg10: 12.5, memory_some_avg10: 0.0, io_full_avg10: 9.0 });
		ledger.record(&hot);
		ledger.record(&sample_at(1000, 1_700_000_001_000));
		let verdict = ledger.close();
		assert_eq!(verdict.flags, vec![ContaminationFlag::IoPressure]);
		assert_eq!(verdict.worst.io_psi_some_avg10, 12.5);
	}

	#[test]
	fn a_wall_clock_step_flags_the_window() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), None);
		ledger.record(&sample_at(0, 1_700_000_000_000));
		// 45 s of monotonic time, 75 s of wall time: something stepped the clock, and
		// the 45 s between two samples is past [`ContaminationPolicy::default`]'s 30 s.
		ledger.record(&sample_at(45_000, 1_700_000_075_000));
		let verdict = ledger.close();
		assert!(verdict.flags.contains(&ContaminationFlag::ClockStep), "{:?}", verdict.flags);
		assert!(verdict.flags.contains(&ContaminationFlag::SampleGap), "{:?}", verdict.flags);
		assert_eq!(verdict.worst.clock_step_ms, 30_000);
	}

	#[test]
	fn swap_major_faults_and_oom_are_window_flags() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), None);
		let mut first = sample_at(0, 1_700_000_000_000);
		first.swap_out_pages = 100;
		first.major_faults = 10;
		first.oom_kills = 0;
		let mut last = sample_at(10_000, 1_700_000_010_000);
		last.swap_out_pages = 100 + 5_000;
		last.major_faults = 10 + 8_000;
		last.oom_kills = 1;
		ledger.record(&first);
		ledger.record(&last);
		let verdict = ledger.close();
		assert!(verdict.flags.contains(&ContaminationFlag::SwapActivity), "{:?}", verdict.flags);
		assert!(verdict.flags.contains(&ContaminationFlag::MajorFaults), "{:?}", verdict.flags);
		assert!(verdict.flags.contains(&ContaminationFlag::OomKill), "{:?}", verdict.flags);
	}

	#[test]
	fn a_foreign_writer_is_measured_not_guessed() {
		let device = StoreDevice { name: "nvme0n1p2".to_string(), mount_point: "/".to_string(), source: "mount_source".to_string() };
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), Some(device));
		ledger.record(&device_sample_at(0, 1_000));
		// The run acknowledged 1 MiB; the device moved 100 MiB.
		ledger.record(&device_sample_at(10_000, 1_000 + 200_000));
		ledger.set_acknowledged_bytes(1_048_576);
		let verdict = ledger.close();
		assert_eq!(verdict.device_written_bytes, Some(102_400_000));
		assert_eq!(verdict.foreign_write_bytes, Some(101_351_424));
		assert!(verdict.flags.contains(&ContaminationFlag::ForeignWriter), "{:?}", verdict.flags);
	}

	#[test]
	fn a_run_that_wrote_everything_is_not_a_foreign_writer() {
		let device = StoreDevice { name: "nvme0n1p2".to_string(), mount_point: "/".to_string(), source: "mount_source".to_string() };
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), Some(device));
		ledger.record(&device_sample_at(0, 1_000));
		ledger.record(&device_sample_at(10_000, 1_000 + 2_048));
		ledger.set_acknowledged_bytes(1_048_576);
		let verdict = ledger.close();
		assert!(verdict.clean(), "{:?}", verdict.flags);
		assert_eq!(verdict.foreign_write_bytes, Some(0));
	}

	#[test]
	fn an_unsampled_window_is_not_clean() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), None);
		let verdict = ledger.close();
		assert_eq!(verdict.flags, vec![ContaminationFlag::SamplerUnavailable]);
		assert!(!verdict.clean());
		let mut partial = ContaminationLedger::new(ContaminationPolicy::default(), None);
		partial.record(&sample_at(0, 1_700_000_000_000));
		partial.note_sampler_unavailable();
		assert!(!partial.close().clean(), "a noted sampler failure survives close");
	}

	#[test]
	fn no_psi_falls_back_to_load_average() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::default(), None);
		let mut blind = sample_at(0, 1_700_000_000_000);
		blind.pressure = None;
		blind.load_avg = [40.0, 40.0, 40.0];
		ledger.record(&blind);
		let verdict = ledger.close();
		assert!(!verdict.psi_capable);
		assert!(verdict.flags.contains(&ContaminationFlag::LoadAverage), "{:?}", verdict.flags);
	}

	#[test]
	fn flush_check_passes_when_the_device_caught_up() {
		let evidence = FlushEvidence { device: Some("nvme0n1p2".to_string()), device_written_bytes: Some(900_000_000), acknowledged_bytes: 1_000_000_000, dirty_kib_at_close: 200_000 };
		let result = evaluate_flush_check(&evidence, 0);
		assert!(result.passed, "{result:?}");
		assert!(!result.blocks(StrictMode::Strict));
		assert_eq!(result.name, "flush_observed");
	}

	#[test]
	fn flush_check_fails_when_the_went_somewhere_else() {
		let evidence = FlushEvidence { device: Some("nvme0n1p2".to_string()), device_written_bytes: Some(10_000_000), acknowledged_bytes: 1_000_000_000, dirty_kib_at_close: 0 };
		let result = evaluate_flush_check(&evidence, 1_048_576);
		assert_eq!(result.failure, Some(FlushFailureKind::WritesNotObserved));
		assert!(result.blocks(StrictMode::Strict));
	}

	#[test]
	fn an_unresolved_device_fails_closed_in_strict_mode() {
		let evidence = FlushEvidence { device: None, device_written_bytes: None, acknowledged_bytes: 1_000, dirty_kib_at_close: 0 };
		let result = evaluate_flush_check(&evidence, 0);
		assert_eq!(result.failure, Some(FlushFailureKind::DeviceUnresolved));
		assert_eq!(result.skipped, Some("device_unresolved"));
		assert!(result.blocks(StrictMode::Strict), "unverified is not evidence of a flush");
		assert!(!result.blocks(StrictMode::Off), "off is advisory over the same numbers");

		let unreadable = FlushEvidence { device: Some("nvme0n1p2".to_string()), device_written_bytes: None, acknowledged_bytes: 1_000, dirty_kib_at_close: 0 };
		assert_eq!(evaluate_flush_check(&unreadable, 0).failure, Some(FlushFailureKind::SamplerUnavailable));
	}

	#[test]
	fn a_dirty_backlog_at_close_is_not_a_flush() {
		let evidence = FlushEvidence { device: Some("nvme0n1p2".to_string()), device_written_bytes: Some(1_000_000_000_000), acknowledged_bytes: 1_000_000_000, dirty_kib_at_close: 4_000_000 };
		let result = evaluate_flush_check(&evidence, 0);
		assert_eq!(result.failure, Some(FlushFailureKind::DirtyBacklog));
		assert!(result.blocks(StrictMode::Strict));
	}

	#[test]
	fn strict_mode_parses_only_its_two_values() {
		assert_eq!(StrictMode::parse("strict").expect("strict"), StrictMode::Strict);
		assert_eq!(StrictMode::parse("off").expect("off"), StrictMode::Off);
		let error = StrictMode::parse("yes").expect_err("not a mode");
		assert!(error.to_string().contains("`off` or `strict`"), "{error}");
	}

	#[test]
	fn sampler_thread_collects_and_stops_promptly() {
		let fixture = Fixture::new("thread");
		host_fixture(&fixture);
		let sampler = SystemSampler::at(fixture.root(), Some("nvme0n1p2".to_string()));
		let thread = SamplerThread::start(sampler, std::time::Duration::from_millis(20)).expect("start");
		std::thread::sleep(std::time::Duration::from_millis(110));
		let started = std::time::Instant::now();
		let shutdown = thread.stop().expect("stop");
		assert!(shutdown.samples.len() >= 3, "samples: {}", shutdown.samples.len());
		assert_eq!(shutdown.failures, 0);
		assert!(started.elapsed() < std::time::Duration::from_millis(150), "stop waited out the interval");
		assert!(shutdown.samples.windows(2).all(|pair| pair[0].monotonic_ms <= pair[1].monotonic_ms));
	}

	#[test]
	fn artifacts_carry_numbers_and_no_paths() {
		let mut ledger = ContaminationLedger::new(ContaminationPolicy::strict(), Some(StoreDevice { name: "nvme0n1p2".to_string(), mount_point: "/mnt/deep/secret/path".to_string(), source: "mount_source".to_string() }));
		ledger.record(&device_sample_at(0, 1_000));
		ledger.record(&device_sample_at(1_000, 2_000));
		ledger.set_acknowledged_bytes(131_072);
		let verdict = ledger.close();
		let verdict_json = verdict.to_json().expect("verdict json");
		let evidence = FlushEvidence::from_verdict(&verdict, 12);
		let flush_json = evaluate_flush_check(&evidence, 0).to_json().expect("flush json");
		for artifact in [&verdict_json, &flush_json] {
			assert!(!artifact.contains("secret"), "artifact leaked a path: {artifact}");
			assert!(!artifact.contains("home"), "artifact leaked a home path: {artifact}");
			assert!(artifact.contains("nvme0n1p2"), "artifact lost the device: {artifact}");
		}
		let artifact: serde_json::Value = serde_json::from_str(&verdict_json).expect("parse");
		let keys: Vec<&str> = artifact.as_object().expect("object").keys().map(String::as_str).collect();
		assert!(keys.contains(&"flags") && keys.contains(&"device_written_bytes") && keys.contains(&"psi_capable"), "{keys:?}");
	}
}
