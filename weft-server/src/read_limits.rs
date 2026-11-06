//! The caps on what one read may return, and the deadline it must finish in
//! (ROB-12, 1.0 plan decision 12).
//!
//! Every read here answers in full or not at all: there is no streaming, no
//! partial result and no resume, so the whole response is materialised before
//! the first byte goes out. A bound therefore has to be decided *before* the
//! work starts — a request that would return more than a client can hold, or
//! that would keep a worker busy past a deadline, is refused rather than
//! attempted. Before this module each read carried its own bound (or none):
//! the interpolated grid had a cap, the JSON point reads had a page limit, and
//! the Arrow, CSV, Parquet, value-range, `at` and stored-downsample reads had
//! whatever the requested window happened to contain.
//!
//! Five numbers, one meaning: a read is answerable when it fits all of them.
//!
//! * [`DEFAULT_PAGE_ROWS`] / [`MAX_PAGE_ROWS`] — the page a paginated read
//!   returns, and the largest page a client may ask for. The *default* is part
//!   of the API contract rather than a knob: a server that quietly changed it
//!   would change everyone's responses, so only the ceiling is configurable.
//! * [`MAX_READ_ROWS`] — the rows of any single read, paginated or not. A
//!   window of a billion stored rows is a query-shape mistake, not a page.
//! * [`MAX_DOWNSAMPLE_BUCKETS`] — the buckets of a downsample. The output grid,
//!   not the input, is what grows without bound here: a nanosecond resolution
//!   over a decade is a request for 3×10¹⁷ buckets, and with `fill` set every
//!   one of them is emitted.
//! * [`MAX_AT_MULTI_INSTANTS`] — the instants of one multi-instant lookup.
//! * [`DEFAULT_READ_TIMEOUT`] — how long the server waits for one read before
//!   it stops. The deadline is what bounds work that passes the other checks
//!   but is slower than expected (a cold page cache, a huge window, a
//!   contended disk): the read may still finish behind the response — a
//!   deadline stops the server *waiting*, it does not cancel a read already
//!   handed to a blocking thread — but no client is left holding a connection
//!   that will never answer.
//!
//! The response codes are decision 12's: `422 result_too_large` for a request
//! that names a page larger than the ceiling, or whose result would be larger
//! than a cap, and `504 query_timeout` for a read the server stopped waiting
//! for. Both carry the limit and the number that crossed it in the detail, so
//! the client can tell "ask for less" from "the server is slow".

use std::time::Duration;

use crate::error::{ApiError, Code};

/// Rows a paginated read returns when the request does not name a page size.
pub const DEFAULT_PAGE_ROWS: usize = 10_000;
/// The largest page size a request may name.
pub const MAX_PAGE_ROWS: usize = 100_000;
/// The most rows one read may return, whether it is paginated or not.
pub const MAX_READ_ROWS: usize = 5_000_000;
/// The most buckets one downsample may return.
pub const MAX_DOWNSAMPLE_BUCKETS: usize = 1_000_000;
/// The most instants one multi-instant (`POST /api/v1/at`) lookup may name.
pub const MAX_AT_MULTI_INSTANTS: usize = 10_000;
/// How long one read may take before the server stops waiting for it.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest page size this server answers with.
pub const MAX_PAGE_ROWS_ENV: &str = "WEFT_MAX_PAGE_ROWS";
/// The most rows one read may return.
pub const MAX_READ_ROWS_ENV: &str = "WEFT_MAX_READ_ROWS";
/// The most buckets one downsample may return.
pub const MAX_DOWNSAMPLE_BUCKETS_ENV: &str = "WEFT_MAX_DOWNSAMPLE_BUCKETS";
/// The most instants one multi-instant lookup may name.
pub const MAX_AT_MULTI_INSTANTS_ENV: &str = "WEFT_MAX_AT_MULTI_INSTANTS";
/// How many seconds one read may take.
pub const READ_TIMEOUT_SECS_ENV: &str = "WEFT_READ_TIMEOUT_SECS";

/// The read caps in force for one server process, read from the environment
/// once at startup (as every other setting here) and carried on
/// [`AppState`](crate::AppState).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLimits {
	page_max: usize,
	read_rows_max: usize,
	buckets_max: usize,
	instants_max: usize,
	read_timeout: Duration,
}

impl Default for ReadLimits {
	fn default() -> Self {
		Self::defaults()
	}
}

impl ReadLimits {
	/// The shipped defaults: a 10 000-row page, at most 100 000 rows to a page,
	/// 5 000 000 rows to a read, 1 000 000 buckets to a downsample, 10 000
	/// instants to a multi lookup, and a 30-second deadline on one read.
	#[must_use]
	pub const fn defaults() -> Self {
		Self { page_max: MAX_PAGE_ROWS, read_rows_max: MAX_READ_ROWS, buckets_max: MAX_DOWNSAMPLE_BUCKETS, instants_max: MAX_AT_MULTI_INSTANTS, read_timeout: DEFAULT_READ_TIMEOUT }
	}

	/// Limits with every cap named (tests, embedders).
	#[must_use]
	pub const fn new(page_max: usize, read_rows_max: usize, buckets_max: usize, instants_max: usize, read_timeout: Duration) -> Self {
		Self { page_max, read_rows_max, buckets_max, instants_max, read_timeout }
	}

	/// These limits with a different read deadline (tests, embedders).
	#[must_use]
	pub const fn with_read_timeout(mut self, timeout: Duration) -> Self {
		self.read_timeout = timeout;
		self
	}

	/// These limits with a different row cap (tests, embedders).
	#[must_use]
	pub const fn with_read_rows_max(mut self, rows: usize) -> Self {
		self.read_rows_max = rows;
		self
	}

	/// These limits with a different page ceiling (tests, embedders).
	#[must_use]
	pub const fn with_page_max(mut self, rows: usize) -> Self {
		self.page_max = rows;
		self
	}

	/// The limits the five `WEFT_*` values describe (`None` for an unset
	/// variable, which leaves that cap at its default).
	///
	/// # Errors
	///
	/// A message naming the variable when a value is not a positive integer, so
	/// a malformed setting fails startup instead of silently falling back to a
	/// default the operator did not ask for. Zero is refused: a cap of zero
	/// answers nothing, which is a mistake and not a configuration.
	pub fn from_env_values(page_max: Option<&str>, read_rows_max: Option<&str>, buckets_max: Option<&str>, instants_max: Option<&str>, read_timeout_secs: Option<&str>) -> Result<Self, String> {
		let mut limits = Self::defaults();
		if let Some(raw) = page_max {
			limits.page_max = parse_positive(MAX_PAGE_ROWS_ENV, raw)?;
		}
		if let Some(raw) = read_rows_max {
			limits.read_rows_max = parse_positive(MAX_READ_ROWS_ENV, raw)?;
		}
		if let Some(raw) = buckets_max {
			limits.buckets_max = parse_positive(MAX_DOWNSAMPLE_BUCKETS_ENV, raw)?;
		}
		if let Some(raw) = instants_max {
			limits.instants_max = parse_positive(MAX_AT_MULTI_INSTANTS_ENV, raw)?;
		}
		if let Some(raw) = read_timeout_secs {
			limits.read_timeout = Duration::from_secs(parse_positive(READ_TIMEOUT_SECS_ENV, raw)? as u64);
		}
		Ok(limits)
	}

	/// The limits the process environment describes; see
	/// [`from_env_values`](Self::from_env_values).
	///
	/// # Errors
	///
	/// A set variable whose value is not a positive integer.
	pub fn from_env() -> Result<Self, String> {
		Self::from_env_values(std::env::var(MAX_PAGE_ROWS_ENV).ok().as_deref(), std::env::var(MAX_READ_ROWS_ENV).ok().as_deref(), std::env::var(MAX_DOWNSAMPLE_BUCKETS_ENV).ok().as_deref(), std::env::var(MAX_AT_MULTI_INSTANTS_ENV).ok().as_deref(), std::env::var(READ_TIMEOUT_SECS_ENV).ok().as_deref())
	}

	/// The page ceiling in force.
	#[must_use]
	pub const fn page_max(&self) -> usize {
		self.page_max
	}

	/// The row cap of one read.
	#[must_use]
	pub const fn read_rows_max(&self) -> usize {
		self.read_rows_max
	}

	/// The bucket cap of one downsample.
	#[must_use]
	pub const fn downsample_buckets_max(&self) -> usize {
		self.buckets_max
	}

	/// The instant cap of one multi lookup.
	#[must_use]
	pub const fn at_multi_instants_max(&self) -> usize {
		self.instants_max
	}

	/// How long one read may take.
	#[must_use]
	pub const fn read_timeout(&self) -> Duration {
		self.read_timeout
	}

	/// The page size to serve for a request that named `requested` rows, or
	/// [`DEFAULT_PAGE_ROWS`] when it named none — never more than
	/// [`page_max`](Self::page_max).
	///
	/// The clamp is the point: an operator who sets `WEFT_MAX_PAGE_ROWS` to 10
	/// to keep responses small would otherwise get the 10,000-row default on
	/// every request that names no page size, which is the opposite of what
	/// they asked for. The ceiling caps the default; it does not raise it.
	///
	/// `parameter` is the query parameter the caller read the size from
	/// (`"limit"`, `"take"`), so the refusal names the thing the client should
	/// change.
	///
	/// # Errors
	///
	/// `422 result_too_large` when the named page is larger than
	/// [`page_max`](Self::page_max) — before the read, so an oversized page
	/// costs no work at all.
	pub fn page_size(&self, requested: Option<usize>, parameter: &'static str) -> Result<usize, ApiError> {
		match requested {
			Some(rows) if rows > self.page_max => Err(refusal(format!("a page of {rows} rows is over this server's `{parameter}` cap of {} rows (set {MAX_PAGE_ROWS_ENV} to change it)", self.page_max), self.page_max)),
			Some(rows) => Ok(rows),
			None => Ok(DEFAULT_PAGE_ROWS.min(self.page_max)),
		}
	}

	/// Refuse a read whose result is `rows` rows wide.
	///
	/// `subject` names the read in the message ("this range read", "this
	/// value-range read"); `paginate_with` is the client's knob that can shrink
	/// it (`Some("limit")`), and `None` for a read with no page to shrink — a
	/// whole-window export, whose only lever is a narrower window.
	///
	/// # Errors
	///
	/// `422 result_too_large` when `rows` exceeds
	/// [`read_rows_max`](Self::read_rows_max).
	pub fn check_rows(&self, rows: usize, subject: &str, paginate_with: Option<&'static str>) -> Result<(), ApiError> {
		if rows > self.read_rows_max {
			let way_out = paginate_with.map_or_else(|| "narrow the window — an export of a whole window has no page to shrink".to_string(), |parameter| format!("narrow the window or paginate with `{parameter}`"));
			return Err(refusal(format!("{subject} returns {rows} rows, over this server's cap of {} rows per read (set {MAX_READ_ROWS_ENV} to change it; {way_out})", self.read_rows_max), self.read_rows_max));
		}
		Ok(())
	}

	/// Refuse a downsample whose output grid is `buckets` wide.
	///
	/// # Errors
	///
	/// `422 result_too_large` when `buckets` exceeds
	/// [`downsample_buckets_max`](Self::downsample_buckets_max).
	pub fn check_downsample_buckets(&self, buckets: u128) -> Result<(), ApiError> {
		if buckets > self.buckets_max as u128 {
			return Err(refusal(format!("this downsample returns {buckets} buckets, over this server's cap of {} buckets (set {MAX_DOWNSAMPLE_BUCKETS_ENV} to change it; a coarser `resolution` or a narrower window fits)", self.buckets_max), self.buckets_max));
		}
		Ok(())
	}

	/// Refuse a multi-instant lookup that names `instants` timestamps.
	///
	/// # Errors
	///
	/// `422 result_too_large` when `instants` exceeds
	/// [`at_multi_instants_max`](Self::at_multi_instants_max).
	pub fn check_at_multi_instants(&self, instants: usize) -> Result<(), ApiError> {
		if instants > self.instants_max {
			return Err(refusal(format!("this lookup names {instants} instants, over this server's cap of {} per request (set {MAX_AT_MULTI_INSTANTS_ENV} to change it; split the list across requests)", self.instants_max), self.instants_max));
		}
		Ok(())
	}

	/// The `504 query_timeout` for a read this server stopped waiting for.
	///
	/// A `5xx`, so the client gets the code's fixed sentence and this detail is
	/// the logged cause — the operator's number, not the caller's.
	#[must_use]
	pub fn deadline_exceeded(&self) -> ApiError {
		ApiError::server(Code::QueryTimeout, format!("the read did not finish within this server's {} s deadline (set {READ_TIMEOUT_SECS_ENV} to change it); a narrower window reads less", self.read_timeout.as_secs()))
	}

	/// The deadline as a whole number of seconds, for the startup log line.
	#[must_use]
	pub const fn read_timeout_secs(&self) -> u64 {
		self.read_timeout.as_secs()
	}
}

/// Run one stored read under this server's read deadline ([`ReadLimits::read_timeout`]).
///
/// The read is handed in as a future that has already mapped its own failure into an
/// [`ApiError`], because each call site classifies differently: a segment-store read goes
/// through [`crate::error::classify_read`] (a poisoned state is not a timeout), an Arrow
/// encode does not. On expiry the caller gets [`ReadLimits::deadline_exceeded`] — a `504`
/// carrying `query_timeout` — and the response stops being waited for.
///
/// A deadline bounds the *wait*, not the work: a read already handed to a blocking thread
/// runs to completion whatever this returns, which is why every row and bucket ceiling is
/// checked again before a body is serialized, and why the ceilings — not the timeout —
/// are what protects the box.
///
/// The read's own failure and the timeout are both `ApiError`, so the result is
/// flattened: a caller's `?` sees one error type and the `504` cannot be mistaken
/// for the read's own status.
///
/// # Errors
///
/// The read's own error, or the `504` of [`ReadLimits::deadline_exceeded`] when the
/// deadline passes first.
pub async fn within_deadline<T, F>(limits: &ReadLimits, read: F) -> Result<T, ApiError>
where
	F: std::future::Future<Output = Result<T, ApiError>>,
{
	tokio::time::timeout(limits.read_timeout(), read).await.map_err(|_| limits.deadline_exceeded())?
}

/// The number of grid steps of `step_nanos` between the inclusive bounds
/// `start_nanos` and `end_nanos` — the bucket count a downsample over that
/// window produces, known before any data is read.
///
/// Zero for an empty or inverted window, and for a non-positive step (which
/// [`BucketWidth`](weft_reduce::BucketWidth) cannot produce). The arithmetic is
/// `i128` because the bounds are the whole `i64` nanosecond range: a decade at
/// nanosecond resolution is 3×10¹⁷ buckets, which does not fit an `i64` count
/// of nothing at all — and it is exactly the request the cap exists to refuse.
#[must_use]
pub const fn grid_buckets(start_nanos: i128, end_nanos: i128, step_nanos: i128) -> u128 {
	if step_nanos <= 0 || end_nanos < start_nanos {
		return 0;
	}
	((end_nanos - start_nanos) / step_nanos).unsigned_abs() + 1
}

/// The `422 result_too_large` shared by the four checks, carrying the cap in the
/// envelope's `limit` field as the interpolation cap's refusal does.
fn refusal(message: String, limit: usize) -> ApiError {
	ApiError::new(Code::ResultTooLarge, message).with_limit(limit as u64)
}

/// Parse a cap: a positive whole number, or a message naming the variable.
/// A cap that parses but is zero is refused rather than honoured.
///
/// `WEFT_MAX_PAGE_ROWS=0` would leave the server serving empty pages to every
/// request, which reads as a working server with a broken client. A variable
/// the operator means to disable is left unset, taking the contract default;
/// a set variable is an instruction and an instruction to return nothing is a
/// mistake at the keyboard.
fn parse_positive(name: &str, raw: &str) -> Result<usize, String> {
	let value: u128 = raw.trim().parse().map_err(|e| format!("{name}={raw:?} is not a positive whole number: {e}"))?;
	let Ok(count) = usize::try_from(value) else {
		return Err(format!("{name}={raw:?} is not a positive whole number of rows, buckets or instants that this server can count"));
	};
	if count == 0 {
		return Err(format!("{name}={raw:?} would cap a read at nothing — leave the variable unset for the default, or name a positive count"));
	}
	Ok(count)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_unset_variable_leaves_its_cap_at_the_default() {
		let limits = ReadLimits::from_env_values(None, None, None, None, None).expect("no variables set");
		assert_eq!(limits, ReadLimits::defaults());
		assert_eq!(limits.page_max(), 100_000);
		assert_eq!(limits.read_rows_max(), 5_000_000);
		assert_eq!(limits.downsample_buckets_max(), 1_000_000);
		assert_eq!(limits.at_multi_instants_max(), 10_000);
		assert_eq!(limits.read_timeout(), Duration::from_secs(30));
	}

	#[test]
	fn a_set_variable_moves_only_its_own_cap() {
		let limits = ReadLimits::from_env_values(Some("500"), None, Some("20"), Some("3"), Some("5")).expect("valid values");
		assert_eq!(limits.page_max(), 500);
		assert_eq!(limits.read_rows_max(), 5_000_000);
		assert_eq!(limits.downsample_buckets_max(), 20);
		assert_eq!(limits.at_multi_instants_max(), 3);
		assert_eq!(limits.read_timeout(), Duration::from_secs(5));
	}

	#[test]
	fn a_malformed_or_zero_cap_names_the_variable_and_stops_startup() {
		for (raw, name) in [("0", MAX_PAGE_ROWS_ENV), ("-1", MAX_READ_ROWS_ENV), ("ten", MAX_DOWNSAMPLE_BUCKETS_ENV), ("", MAX_AT_MULTI_INSTANTS_ENV), ("0", READ_TIMEOUT_SECS_ENV)] {
			let error = match name {
				MAX_PAGE_ROWS_ENV => ReadLimits::from_env_values(Some(raw), None, None, None, None),
				MAX_READ_ROWS_ENV => ReadLimits::from_env_values(None, Some(raw), None, None, None),
				MAX_DOWNSAMPLE_BUCKETS_ENV => ReadLimits::from_env_values(None, None, Some(raw), None, None),
				MAX_AT_MULTI_INSTANTS_ENV => ReadLimits::from_env_values(None, None, None, Some(raw), None),
				_ => ReadLimits::from_env_values(None, None, None, None, Some(raw)),
			};
			let message = error.expect_err(&format!("{name}={raw:?} must be refused"));
			assert!(message.contains(name), "{message} must name {name}");
		}
	}

	#[test]
	fn a_page_at_the_ceiling_answers_and_one_above_is_refused_before_any_read() {
		let limits = ReadLimits::defaults();
		assert_eq!(limits.page_size(Some(100_000), "limit").expect("at the ceiling"), 100_000);
		assert_eq!(limits.page_size(Some(0), "limit").expect("an empty page is what was asked for"), 0);
		let error = limits.page_size(Some(100_001), "limit").expect_err("one above the ceiling");
		assert_eq!(error.code(), Code::ResultTooLarge);
		assert_eq!(error.message(), "a page of 100001 rows is over this server's `limit` cap of 100000 rows (set WEFT_MAX_PAGE_ROWS to change it)");
	}

	#[test]
	fn a_request_that_names_no_page_gets_the_contract_default_or_the_ceiling() {
		let limits = ReadLimits::defaults();
		assert_eq!(limits.page_size(None, "take").expect("no page named"), DEFAULT_PAGE_ROWS, "the ordinary server pages at the documented default");
		// An operator who lowered the ceiling below the default meant smaller
		// pages; serving them the default anyway would be the surprise.
		let lowered = ReadLimits::from_env_values(Some("10"), None, None, None, None).expect("a small ceiling");
		assert_eq!(lowered.page_size(None, "take").expect("no page named"), 10, "the ceiling caps the default, it does not raise it");
	}

	#[test]
	fn a_result_over_the_row_cap_is_refused_with_the_number_that_crossed_it() {
		let limits = ReadLimits::defaults().with_read_rows_max(1_000);
		assert!(limits.check_rows(1_000, "this range read", Some("limit")).is_ok());
		let error = limits.check_rows(1_001, "this range read", Some("limit")).expect_err("one over");
		assert_eq!(error.code(), Code::ResultTooLarge);
		assert_eq!(error.message(), "this range read returns 1001 rows, over this server's cap of 1000 rows per read (set WEFT_MAX_READ_ROWS to change it; narrow the window or paginate with `limit`)");
		let export = limits.check_rows(1_001, "this range export", None).expect_err("an export has no page");
		assert!(export.message().contains("no page to shrink"), "{}", export.message());
	}

	#[test]
	fn a_bucket_grid_is_counted_from_its_bounds_before_the_read() {
		let second: i128 = 1_000_000_000;
		assert_eq!(grid_buckets(0, 0, second), 1, "an inclusive one-instant window is one bucket");
		assert_eq!(grid_buckets(0, 5 * second, second), 6);
		assert_eq!(grid_buckets(0, 4_999_999_999, second), 5, "a partial trailing step is not a bucket");
		assert_eq!(grid_buckets(10, 5, second), 0, "an inverted window has no grid");
		assert_eq!(grid_buckets(0, 10, 0), 0, "a zero step cannot index a grid");
	}

	#[test]
	fn a_decade_at_nanosecond_resolution_is_counted_without_overflowing() {
		let decade_nanos: i128 = 315_576_000 * 1_000_000_000;
		let buckets = grid_buckets(0, decade_nanos, 1);
		// The count is pinned exactly rather than as "some huge number": 3.16e17
		// buckets fits an `i64` with room to spare, so a magnitude assertion here
		// would pass for a wrapped count too, and the wrapped count is what the
		// `i128` arithmetic exists to prevent.
		assert_eq!(buckets, u128::try_from(decade_nanos).unwrap() + 1, "a one-nanosecond grid over a decade counts every bucket, inclusive of the first");
		let error = ReadLimits::defaults().check_downsample_buckets(buckets).expect_err("the widest request a client can name");
		assert_eq!(error.code(), Code::ResultTooLarge);
		assert!(error.message().contains(&buckets.to_string()), "{buckets} must appear in {:?}", error.message());
	}

	#[test]
	fn the_whole_nanosecond_range_counts_to_the_i128_answer_and_not_a_wrapped_one() {
		// The bounds a client gets by omitting `start`/`end`: a span of 2^64 - 1
		// nanoseconds, whose bucket count at a 1 ns step does not fit `i64` or
		// even `u64`. A wrapped count here would read as a small, legal grid.
		let buckets = grid_buckets(i128::from(i64::MIN), i128::from(i64::MAX), 1);
		assert_eq!(buckets, 1_u128 << 64, "the whole i64 nanosecond range at a 1 ns step is 2^64 buckets");
		assert!(ReadLimits::defaults().check_downsample_buckets(buckets).is_err(), "and that request is refused, never attempted");
	}

	#[test]
	fn a_multi_lookup_is_capped_on_the_instants_it_names() {
		let limits = ReadLimits::defaults().with_page_max(1);
		assert!(limits.check_at_multi_instants(10_000).is_ok());
		let error = limits.check_at_multi_instants(10_001).expect_err("one over");
		assert_eq!(error.code(), Code::ResultTooLarge);
		assert!(error.message().starts_with("this lookup names 10001 instants"), "{}", error.message());
	}

	/// The `504` is pinned here rather than over the wire because a pending read
	/// has to be created on purpose to be timed out: `timeout` polls the read
	/// before it inspects the clock, so a read that completes without ever waiting
	/// is served no matter how short the deadline is. That is the deadline's
	/// contract — it bounds the wait, never a result already produced — and the
	/// wire-level consequence is pinned in `tests/read_limits.rs`.
	#[tokio::test]
	async fn a_read_still_waiting_when_its_deadline_passes_is_refused_with_504() {
		let limits = ReadLimits::defaults().with_read_timeout(Duration::from_millis(5));
		let outcome = within_deadline(&limits, async {
			tokio::time::sleep(Duration::from_secs(60)).await;
			Ok::<(), ApiError>(())
		})
		.await;
		let error = match outcome {
			Ok(()) => panic!("a read that outlived its deadline was served anyway"),
			Err(error) => error,
		};
		assert_eq!(error.code(), Code::QueryTimeout);
		assert_eq!(error.code().status(), axum::http::StatusCode::GATEWAY_TIMEOUT, "the status follows from the code");
		assert!(error.message().contains("deadline"), "{}", error.message());
	}

	#[tokio::test]
	async fn a_read_that_never_waits_is_served_under_a_zero_deadline() {
		let limits = ReadLimits::defaults().with_read_timeout(Duration::ZERO);
		within_deadline(&limits, async { Ok::<u8, ApiError>(7) }).await.map(|value| assert_eq!(value, 7)).expect("a read that did not wait has not exceeded anything");
	}

	#[test]
	fn the_deadline_is_a_504_whose_cause_names_the_wait_and_the_variable() {
		let limits = ReadLimits::defaults().with_read_timeout(Duration::from_secs(7));
		let error = limits.deadline_exceeded();
		assert_eq!(error.code(), Code::QueryTimeout);
		assert_eq!(error.message(), "the read did not finish within this server's 7 s deadline (set WEFT_READ_TIMEOUT_SECS to change it); a narrower window reads less", "the cause is what the operator reads in the log; the wire body carries the code's fixed sentence");
	}
}
