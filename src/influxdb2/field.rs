#![allow(dead_code)]
use core::fmt::Formatter;
use std::fmt::Display;

#[derive(Debug, Clone)]
pub enum InfluxField {
	String(String),
	Integer(i64),
	Float(f64),
	Boolean(bool),
}

impl Display for InfluxField {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			InfluxField::String(value) => write!(f, "{}", value),
			InfluxField::Integer(value) => write!(f, "{}", value),
			InfluxField::Float(value) => write!(f, "{}", value),
			InfluxField::Boolean(value) => write!(f, "{}", value),
		}
	}
}
