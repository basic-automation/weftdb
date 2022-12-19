#![allow(dead_code)]
use super::timestamp::*;

#[derive(Debug, Clone)]
pub struct InfluxConnection {
	pub base_url: String,
	pub token: String,
	pub org: String,
	pub bucket: String,
	pub precision: Option<InfluxTimestampPrecision>,
}
