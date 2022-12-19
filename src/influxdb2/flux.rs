#![allow(dead_code)]
use chrono::{DateTime, Utc};
use core::fmt::Formatter;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt::Display};

pub struct FluxQuery {
	pub now: Option<DateTime<Utc>>,
	pub params: Option<HashMap<String, String>>,
	pub query: Option<String>,
}

impl FluxQuery {
	pub fn new() -> Self {
		FluxQuery { now: None, params: None, query: None }
	}
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FluxQueryRecord {
	pub result: String,
	pub table: String,
	#[serde(rename(deserialize = "_start"))]
	pub start: String,
	#[serde(rename(deserialize = "_stop"))]
	pub stop: String,
	#[serde(rename(deserialize = "_time"))]
	pub time: String,
	#[serde(rename(deserialize = "_value"))]
	pub value: f64,
	#[serde(rename(deserialize = "_field"))]
	pub field: String,
	#[serde(rename(deserialize = "_measurement"))]
	pub measurement: String,
	pub base: String,
}

pub struct FluxDelete {
	pub start: Option<i64>,
	pub stop: Option<i64>,
	pub predicate: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Copy, Eq)]
pub enum FluxInterpolation {
	None,
	Second,
	Minute,
	Hour,
	Day,
}

impl Display for FluxInterpolation {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			FluxInterpolation::None => write!(f, ""),
			FluxInterpolation::Second => write!(f, "1s"),
			FluxInterpolation::Minute => write!(f, "1m"),
			FluxInterpolation::Hour => write!(f, "1h"),
			FluxInterpolation::Day => write!(f, "1d"),
		}
	}
}
