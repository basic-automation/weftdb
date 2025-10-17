use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use splimes::Point;
use uuid::Uuid;

use crate::{types::database::traits::aspect_structure::AspectStructure, AspectId, Database, Error, Measurement, CACHE, DATABASES};

impl Database {
	pub(crate) fn sanitize_table_name(name: &str) -> String {
		// Replace special characters with underscores
		name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
	}

	#[must_use]
	pub fn measurements_to_points(measurements: &[Measurement]) -> Vec<Point> {
		measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect()
	}
}

pub(crate) fn safe_usize_to_f64(value: usize) -> std::result::Result<f64, Error> {
	let value_u64 = u64::try_from(value).map_err(|_| Error::NumericConversionError(format!("Value {value} does not fit into u64")))?;
	let max_exact = 1u64 << f64::MANTISSA_DIGITS;
	if value_u64 > max_exact {
		return Err(Error::NumericConversionError(format!("Value {value} exceeds f64 precision limit (max exact integer is {max_exact})")));
	}
	#[allow(clippy::cast_precision_loss)]
	Ok(value_u64 as f64)
}

pub(crate) fn safe_i64_to_f64(value: i64) -> std::result::Result<f64, Error> {
	let abs_value = value.checked_abs().ok_or_else(|| Error::NumericConversionError(format!("Value {value} cannot be safely negated")))?;
	let abs_u64 = u64::try_from(abs_value).map_err(|_| Error::NumericConversionError(format!("Value {abs_value} cannot be represented as u64")))?;
	let max_exact = 1u64 << f64::MANTISSA_DIGITS;
	if abs_u64 > max_exact {
		return Err(Error::NumericConversionError(format!("Value {value} exceeds f64 precision limit (max exact integer is {max_exact})")));
	}
	#[allow(clippy::cast_precision_loss)]
	Ok(value as f64)
}

pub(crate) fn safe_ratio(numerator: usize, denominator: usize) -> std::result::Result<f64, Error> {
	if denominator == 0 {
		return Ok(0.0);
	}
	let numerator_f64 = safe_usize_to_f64(numerator)?;
	let denominator_f64 = safe_usize_to_f64(denominator)?;
	Ok(numerator_f64 / denominator_f64)
}
