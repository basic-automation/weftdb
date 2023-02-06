use super::field::*;
use std::collections::HashMap;

pub struct LineProtocol {
	pub measurement: Option<String>,
	pub tags: Option<HashMap<String, String>>,
	pub fields: Option<HashMap<String, InfluxField>>,
	pub timestamp: Option<i64>,
}

impl LineProtocol {
	pub fn new() -> Self {
		LineProtocol { measurement: None, tags: None, fields: None, timestamp: None }
	}

	pub async fn build(&self) -> String {
		let mut line_protocol = String::new();
		if let Some(measurement) = &self.measurement {
			line_protocol.push_str(measurement);
		}
		if let Some(tags) = &self.tags {
			line_protocol.push(',');
			for (tag_name, tag_value) in tags {
				line_protocol.push_str(tag_name);
				line_protocol.push('=');
				line_protocol.push_str(tag_value);
				line_protocol.push(',');
			}
			line_protocol.pop();
		}
		if let Some(fields) = &self.fields {
			line_protocol.push(' ');
			for (field_name, field_value) in fields {
				line_protocol.push_str(field_name);
				line_protocol.push('=');
				match field_value {
					InfluxField::String(value) => {
						line_protocol.push('\"');
						line_protocol.push_str(value);
						line_protocol.push('\"');
					}
					InfluxField::Integer(value) => line_protocol.push_str(&value.to_string()),
					InfluxField::Float(value) => line_protocol.push_str(&value.to_string()),
					InfluxField::Boolean(value) => line_protocol.push_str(&value.to_string()),
				}
				line_protocol.push(',');
			}
			line_protocol.pop();
		}
		if let Some(timestamp) = self.timestamp {
			line_protocol.push(' ');
			line_protocol.push_str(&timestamp.to_string());
		}
		line_protocol
	}
}
