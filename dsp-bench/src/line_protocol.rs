//! `InfluxDB` Line Protocol (ILP) parsing for dataset ingest.
//!
//! ILP is the wire format `InfluxDB`, `QuestDB`, and the TSBS benchmark suite all
//! speak, so parsing it is the cheapest path to **TSBS compatibility** — the
//! roadmap's Phase 1 calls for "TSBS compatibility (via `InfluxDB` Line Protocol)"
//! as a baseline-comparison on-ramp, and Immediate Next Action #5 is "Implement
//! `InfluxDB` Line Protocol ingest".
//!
//! This module is deliberately a *format* parser, not a vendor connector: it
//! turns ILP text into neutral [`LineRecord`]s (and, via [`parse_points`], into
//! `splimes::Point`s the bench harness already consumes). It pulls in no
//! `InfluxDB` client, no network, and no vendor-specific dependency, so it honors
//! the connector hard-constraint — a concrete `InfluxDB` *connector* (talking to a
//! running server) would live in its own crate outside the core.
//!
//! ## Grammar (the subset that matters for ingest)
//!
//! ```text
//! measurement[,tag_key=tag_value...] field_key=field_value[,...] [timestamp]
//! ```
//!
//! - The measurement-and-tag set runs up to the first unescaped, unquoted space;
//!   the field set up to the next; an optional integer timestamp follows.
//! - In the measurement/tag/field-key region, `\,`, `\ ` and `\=` escape the
//!   structural characters. String field values are double-quoted and may
//!   contain spaces and commas; inside them `\"` and `\\` are the escapes.
//! - Field values are typed: `1.5` float, `42i` signed integer, `42u` unsigned,
//!   `t`/`true`/`f`/`false` boolean, `"..."` string.
//! - Blank lines and lines beginning with `#` (after leading spaces) are skipped.
//!
//! The timestamp's unit is caller-declared via [`TimestampPrecision`]; ILP's
//! default is nanoseconds.

use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{DateTime, Utc};
use splimes::Point;

/// The unit a line-protocol integer timestamp is expressed in.
///
/// ILP itself carries no unit; the caller declares it (matching the
/// `--precision` an InfluxDB/TSBS writer was configured with). The default,
/// [`TimestampPrecision::Nanoseconds`], is ILP's own default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimestampPrecision {
	/// Nanoseconds since the Unix epoch (ILP default).
	#[default]
	Nanoseconds,
	/// Microseconds since the Unix epoch.
	Microseconds,
	/// Milliseconds since the Unix epoch.
	Milliseconds,
	/// Seconds since the Unix epoch.
	Seconds,
}

impl TimestampPrecision {
	/// How many nanoseconds one unit of this precision spans.
	const fn nanos_per_unit(self) -> i64 {
		match self {
			Self::Nanoseconds => 1,
			Self::Microseconds => 1_000,
			Self::Milliseconds => 1_000_000,
			Self::Seconds => 1_000_000_000,
		}
	}

	/// Convert a raw line-protocol timestamp in this precision to a UTC instant.
	///
	/// Returns `None` if scaling the value to nanoseconds overflows `i64` (i.e.
	/// the instant falls outside chrono's representable nanosecond range).
	#[must_use]
	pub fn to_datetime(self, raw: i64) -> Option<DateTime<Utc>> {
		raw.checked_mul(self.nanos_per_unit()).map(DateTime::from_timestamp_nanos)
	}
}

/// A typed field value parsed from a line-protocol field set.
///
/// ILP fields are typed by syntax; this preserves that type so a consumer can
/// decide what to do with non-numeric fields rather than silently coercing them.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
	/// A floating-point field (the default for bare numbers).
	Float(f64),
	/// A signed-integer field (the `i` suffix, e.g. `42i`).
	Integer(i64),
	/// An unsigned-integer field (the `u` suffix, e.g. `42u`).
	Unsigned(u64),
	/// A boolean field (`t`/`T`/`true` / `f`/`F`/`false`).
	Boolean(bool),
	/// A string field (double-quoted in the wire form).
	Str(String),
}

impl FieldValue {
	/// The numeric value as a [`BigDecimal`], or `None` for non-numeric fields
	/// (booleans and strings).
	///
	/// `Float`s that are NaN or infinite yield `None`, since they cannot be
	/// represented as a finite decimal measurement.
	#[must_use]
	pub fn as_big_decimal(&self) -> Option<BigDecimal> {
		match self {
			Self::Float(f) => BigDecimal::from_f64(*f),
			Self::Integer(i) => Some(BigDecimal::from(*i)),
			Self::Unsigned(u) => Some(BigDecimal::from(*u)),
			Self::Boolean(_) | Self::Str(_) => None,
		}
	}
}

/// A single parsed line-protocol record.
///
/// Tags and fields preserve their on-wire order so a round-trip or a "first
/// numeric field" projection is deterministic.
#[derive(Debug, Clone, PartialEq)]
pub struct LineRecord {
	/// The measurement (table) name.
	pub measurement: String,
	/// Tag key/value pairs, in wire order. May be empty.
	pub tags: Vec<(String, String)>,
	/// Field key/value pairs, in wire order. Always at least one.
	pub fields: Vec<(String, FieldValue)>,
	/// The raw integer timestamp, in the line's (caller-declared) precision.
	/// `None` when the line omitted it (the writer would substitute server time).
	pub timestamp: Option<i64>,
}

impl LineRecord {
	/// The value of tag `key`, if present.
	#[must_use]
	pub fn tag(&self, key: &str) -> Option<&str> {
		self.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
	}

	/// The value of field `key`, if present.
	#[must_use]
	pub fn field(&self, key: &str) -> Option<&FieldValue> {
		self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
	}

	/// Project this record into a [`Point`] using numeric field `field`.
	///
	/// Returns `None` unless the record carries a timestamp, the named field
	/// exists and is numeric (and finite), and the timestamp is representable.
	#[must_use]
	pub fn to_point(&self, field: &str, precision: TimestampPrecision) -> Option<Point> {
		let raw_ts = self.timestamp?;
		let timestamp = precision.to_datetime(raw_ts)?;
		let value = self.field(field)?.as_big_decimal()?;
		Some(Point { timestamp, value })
	}
}

/// What went wrong parsing a single line, with the 1-based line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
	/// 1-based line number within the input the error occurred on.
	pub line: usize,
	/// Human-readable reason.
	pub message: String,
}

impl std::fmt::Display for ParseError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "line {}: {}", self.line, self.message)
	}
}

impl std::error::Error for ParseError {}

/// Parse a whole line-protocol document into records.
///
/// Blank lines and `#` comment lines are skipped. The first malformed line
/// aborts the parse with a [`ParseError`] carrying its line number.
///
/// # Errors
///
/// Returns the first [`ParseError`] encountered (malformed structure, empty
/// measurement, a field without a value, or an unparseable field/timestamp).
pub fn parse(input: &str) -> Result<Vec<LineRecord>, ParseError> {
	let mut records = Vec::new();
	for (idx, raw_line) in input.lines().enumerate() {
		let line = raw_line.trim_end_matches('\r');
		if is_blank_or_comment(line) {
			continue;
		}
		records.push(parse_line(line).map_err(|message| ParseError { line: idx + 1, message })?);
	}
	Ok(records)
}

/// Parse a line-protocol document and project it straight into `Point`s using a
/// chosen numeric field — the harness's primary on-ramp for TSBS-style data.
///
/// Records lacking a timestamp or the named numeric field are skipped (not an
/// error: an ILP file may interleave many fields/measurements). The result is
/// sorted by timestamp, matching what the interpolation profiles expect.
///
/// # Errors
///
/// Returns a [`ParseError`] if any line is structurally malformed.
pub fn parse_points(input: &str, field: &str, precision: TimestampPrecision) -> Result<Vec<Point>, ParseError> {
	let mut points: Vec<Point> = parse(input)?.iter().filter_map(|r| r.to_point(field, precision)).collect();
	points.sort_by_key(|p| p.timestamp);
	Ok(points)
}

/// True for lines that carry no record: empty/whitespace, or a `#` comment.
fn is_blank_or_comment(line: &str) -> bool {
	let trimmed = line.trim_start();
	trimmed.is_empty() || trimmed.starts_with('#')
}

/// Parse one non-blank, non-comment line. Errors carry only a message; the
/// caller attaches the line number.
fn parse_line(line: &str) -> Result<LineRecord, String> {
	// Top-level split on unescaped, unquoted spaces: [tagset] [fieldset] [ts?].
	let parts = split_unescaped(line, ' ', true);
	if parts.len() < 2 || parts.len() > 3 {
		return Err(format!("expected `measurement[,tags] fields [timestamp]`, found {} space-separated section(s)", parts.len()));
	}

	let (measurement, tags) = parse_measurement_and_tags(&parts[0])?;
	let fields = parse_fields(&parts[1])?;

	let timestamp = match parts.get(2) {
		Some(ts) if !ts.is_empty() => Some(ts.parse::<i64>().map_err(|_| format!("invalid timestamp `{ts}`"))?),
		_ => None,
	};

	Ok(LineRecord { measurement, tags, fields, timestamp })
}

/// Parse the `measurement[,tag=value...]` section.
fn parse_measurement_and_tags(section: &str) -> Result<(String, Vec<(String, String)>), String> {
	let mut iter = split_unescaped(section, ',', false).into_iter();
	let measurement = unescape(&iter.next().unwrap_or_default());
	if measurement.is_empty() {
		return Err("empty measurement name".to_string());
	}
	let tags = iter
		.map(|pair| {
			let kv = split_unescaped(&pair, '=', false);
			match kv.as_slice() {
				[k, v] if !k.is_empty() => Ok((unescape(k), unescape(v))),
				_ => Err(format!("malformed tag `{pair}`")),
			}
		})
		.collect::<Result<Vec<_>, _>>()?;
	Ok((measurement, tags))
}

/// Parse the `field=value[,field=value...]` section (at least one field).
fn parse_fields(section: &str) -> Result<Vec<(String, FieldValue)>, String> {
	let fields = split_unescaped(section, ',', true)
		.into_iter()
		.map(|pair| {
			let kv = split_unescaped(&pair, '=', true);
			match kv.as_slice() {
				[k, v] if !k.is_empty() && !v.is_empty() => Ok((unescape(k), parse_field_value(v)?)),
				_ => Err(format!("malformed field `{pair}`")),
			}
		})
		.collect::<Result<Vec<_>, _>>()?;
	if fields.is_empty() {
		return Err("a line must have at least one field".to_string());
	}
	Ok(fields)
}

/// Type a single raw field value by its line-protocol syntax.
fn parse_field_value(raw: &str) -> Result<FieldValue, String> {
	// String: double-quoted, may contain escaped quotes/backslashes.
	if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
		return Ok(FieldValue::Str(unescape(&raw[1..raw.len() - 1])));
	}
	// Booleans: the full set InfluxDB accepts.
	match raw {
		"t" | "T" | "true" | "True" | "TRUE" => return Ok(FieldValue::Boolean(true)),
		"f" | "F" | "false" | "False" | "FALSE" => return Ok(FieldValue::Boolean(false)),
		_ => {}
	}
	// Integer / unsigned by suffix.
	if let Some(body) = raw.strip_suffix('i') {
		return body.parse::<i64>().map(FieldValue::Integer).map_err(|_| format!("invalid integer field `{raw}`"));
	}
	if let Some(body) = raw.strip_suffix('u') {
		return body.parse::<u64>().map(FieldValue::Unsigned).map_err(|_| format!("invalid unsigned field `{raw}`"));
	}
	// Otherwise a float.
	raw.parse::<f64>().map(FieldValue::Float).map_err(|_| format!("invalid field value `{raw}`"))
}

/// Split `s` on unescaped (and, when `respect_quotes`, unquoted) occurrences of
/// `delim`, preserving the still-escaped text of each segment.
///
/// A backslash escapes the following character (the backslash is kept so a later
/// [`unescape`] pass can resolve it in the right context). When `respect_quotes`
/// is set, a `delim` inside a double-quoted span is not a split point.
fn split_unescaped(s: &str, delim: char, respect_quotes: bool) -> Vec<String> {
	let mut out = Vec::new();
	let mut cur = String::new();
	let mut escaped = false;
	let mut in_quote = false;
	for c in s.chars() {
		if escaped {
			cur.push(c);
			escaped = false;
		} else if c == '\\' {
			cur.push(c);
			escaped = true;
		} else if respect_quotes && c == '"' {
			in_quote = !in_quote;
			cur.push(c);
		} else if c == delim && !in_quote {
			out.push(std::mem::take(&mut cur));
		} else {
			cur.push(c);
		}
	}
	out.push(cur);
	out
}

/// Resolve backslash escapes, dropping the escaping backslash. A trailing lone
/// backslash is kept literally.
fn unescape(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut escaped = false;
	for c in s.chars() {
		if escaped {
			out.push(c);
			escaped = false;
		} else if c == '\\' {
			escaped = true;
		} else {
			out.push(c);
		}
	}
	if escaped {
		out.push('\\');
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_a_full_line_with_tags_fields_and_timestamp() {
		let records = parse("cpu,host=a,region=us load=0.5,count=3i 1577836800000000000").expect("parse");
		assert_eq!(records.len(), 1);
		let r = &records[0];
		assert_eq!(r.measurement, "cpu");
		assert_eq!(r.tag("host"), Some("a"));
		assert_eq!(r.tag("region"), Some("us"));
		assert_eq!(r.field("load"), Some(&FieldValue::Float(0.5)));
		assert_eq!(r.field("count"), Some(&FieldValue::Integer(3)));
		assert_eq!(r.timestamp, Some(1_577_836_800_000_000_000));
	}

	#[test]
	fn parses_line_without_tags_or_timestamp() {
		let records = parse("weather temp=21.3").expect("parse");
		let r = &records[0];
		assert_eq!(r.measurement, "weather");
		assert!(r.tags.is_empty());
		assert_eq!(r.field("temp"), Some(&FieldValue::Float(21.3)));
		assert_eq!(r.timestamp, None);
	}

	#[test]
	fn types_every_field_kind() {
		let r = &parse(r#"m f=1.5,i=7i,u=9u,bt=t,bf=false,s="hi""#).expect("parse")[0];
		assert_eq!(r.field("f"), Some(&FieldValue::Float(1.5)));
		assert_eq!(r.field("i"), Some(&FieldValue::Integer(7)));
		assert_eq!(r.field("u"), Some(&FieldValue::Unsigned(9)));
		assert_eq!(r.field("bt"), Some(&FieldValue::Boolean(true)));
		assert_eq!(r.field("bf"), Some(&FieldValue::Boolean(false)));
		assert_eq!(r.field("s"), Some(&FieldValue::Str("hi".to_string())));
	}

	#[test]
	fn string_fields_may_contain_spaces_commas_and_escaped_quotes() {
		// The space and comma live inside the quoted value and must not split it;
		// the trailing `value=2i` proves the field set resumed correctly.
		let r = &parse(r#"m note="a, b \"c\"",value=2i 10"#).expect("parse")[0];
		assert_eq!(r.field("note"), Some(&FieldValue::Str(r#"a, b "c""#.to_string())));
		assert_eq!(r.field("value"), Some(&FieldValue::Integer(2)));
		assert_eq!(r.timestamp, Some(10));
	}

	#[test]
	fn honors_escaped_commas_spaces_and_equals_in_keys() {
		let r = &parse(r"my\ measurement,tag\=key=tag\,val field\ name=1").expect("parse")[0];
		assert_eq!(r.measurement, "my measurement");
		assert_eq!(r.tag("tag=key"), Some("tag,val"));
		assert_eq!(r.field("field name"), Some(&FieldValue::Float(1.0)));
	}

	#[test]
	fn skips_blank_and_comment_lines() {
		let input = "\n  # a comment\nm v=1 1\n\n#another\nm v=2 2\n";
		let records = parse(input).expect("parse");
		assert_eq!(records.len(), 2);
		assert_eq!(records[0].timestamp, Some(1));
		assert_eq!(records[1].timestamp, Some(2));
	}

	#[test]
	fn reports_the_line_number_of_a_malformed_line() {
		let err = parse("m v=1 1\nthis is not valid line protocol\n").expect_err("must fail");
		assert_eq!(err.line, 2);
	}

	#[test]
	fn rejects_empty_measurement_and_valueless_field() {
		assert!(parse(" v=1").is_err(), "empty measurement");
		assert!(parse("m field").is_err(), "field without a value is malformed");
		assert!(parse("m ").is_err(), "missing field section");
	}

	#[test]
	fn precision_scales_the_timestamp() {
		assert_eq!(TimestampPrecision::Seconds.to_datetime(1).unwrap(), DateTime::from_timestamp_nanos(1_000_000_000));
		assert_eq!(TimestampPrecision::Milliseconds.to_datetime(1).unwrap(), DateTime::from_timestamp_nanos(1_000_000));
		assert_eq!(TimestampPrecision::Microseconds.to_datetime(1).unwrap(), DateTime::from_timestamp_nanos(1_000));
		assert_eq!(TimestampPrecision::Nanoseconds.to_datetime(1).unwrap(), DateTime::from_timestamp_nanos(1));
		// Overflow when scaling seconds to nanoseconds yields None rather than wrapping.
		assert_eq!(TimestampPrecision::Seconds.to_datetime(i64::MAX), None);
	}

	#[test]
	fn projects_records_into_sorted_points_for_a_chosen_field() {
		// Out-of-order timestamps; a record missing `load`; a record missing a
		// timestamp; a non-numeric `load`. Only the two valid numeric+timestamped
		// `load` rows survive, sorted ascending.
		let input = concat!("cpu,host=a load=0.9 2000000000\n", "cpu,host=a load=0.1 1000000000\n", "cpu,host=a other=5i 3000000000\n", "cpu,host=a load=0.5\n", "cpu,host=a load=\"x\" 4000000000\n");
		let points = parse_points(input, "load", TimestampPrecision::Nanoseconds).expect("parse");
		assert_eq!(points.len(), 2);
		assert!(points[0].timestamp < points[1].timestamp);
		assert_eq!(points[0].value, BigDecimal::from_f64(0.1).unwrap());
		assert_eq!(points[1].value, BigDecimal::from_f64(0.9).unwrap());
	}

	#[test]
	fn field_value_numeric_coercion_rejects_non_numbers() {
		assert!(FieldValue::Integer(5).as_big_decimal().is_some());
		assert!(FieldValue::Unsigned(5).as_big_decimal().is_some());
		assert!(FieldValue::Float(1.5).as_big_decimal().is_some());
		assert!(FieldValue::Float(f64::NAN).as_big_decimal().is_none());
		assert!(FieldValue::Float(f64::INFINITY).as_big_decimal().is_none());
		assert!(FieldValue::Boolean(true).as_big_decimal().is_none());
		assert!(FieldValue::Str("x".to_string()).as_big_decimal().is_none());
	}
}
