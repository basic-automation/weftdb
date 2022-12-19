#![allow(dead_code)]
use chrono::Utc;
pub use connection::*;
pub use field::*;
pub use flux::*;
pub use line_protocol::*;
use std::collections::HashMap;
pub use timestamp::*;

mod connection;
mod field;
mod flux;
mod line_protocol;
mod timestamp;

pub struct Influxdb2 {
	pub connection: Option<InfluxConnection>,
	pub measurement: Option<LineProtocol>,
	pub query: Option<FluxQuery>,
}

impl Influxdb2 {
	pub fn new() -> Self {
		Influxdb2 { connection: None, measurement: None, query: None }
	}

	pub async fn connection(&mut self, base_url: &str, token: &str, org: &str, bucket: &str) {
		let connection = InfluxConnection { base_url: base_url.to_string(), token: token.to_string(), org: org.to_string(), bucket: bucket.to_string(), precision: None };
		self.connection = Some(connection);
	}

	pub async fn new_measurement(&mut self, measurement_name: &str) {
		self.measurement = Some(LineProtocol::new());
		if let Some(measurement) = self.measurement.as_mut() {
			measurement.measurement = Some(measurement_name.to_string())
		}
	}

	pub async fn add_tag(&mut self, tag_name: &str, tag_value: &str) {
		match self.measurement.as_mut().unwrap().tags {
			Some(ref mut tags) => {
				tags.insert(tag_name.to_string(), tag_value.to_string());
			}
			None => {
				let mut tags = HashMap::new();
				tags.insert(tag_name.to_string(), tag_value.to_string());
				self.measurement.as_mut().unwrap().tags = Some(tags);
			}
		}
	}

	pub async fn add_field(&mut self, field_name: &str, field_value: InfluxField) {
		match self.measurement.as_mut().unwrap().fields {
			Some(ref mut fields) => {
				fields.insert(field_name.to_string(), field_value);
			}
			None => {
				let mut fields = HashMap::new();
				fields.insert(field_name.to_string(), field_value);
				self.measurement.as_mut().unwrap().fields = Some(fields);
			}
		}
	}

	pub async fn add_timestamp(&mut self, timestamp: InfluxTimestamp) {
		match timestamp {
			InfluxTimestamp::Now => self.measurement.as_mut().unwrap().timestamp = Some(Utc::now().timestamp()),
			InfluxTimestamp::DateTime(timestamp) => self.measurement.as_mut().unwrap().timestamp = Some(timestamp.timestamp()),
			InfluxTimestamp::Unix(timestamp) => self.measurement.as_mut().unwrap().timestamp = Some(timestamp),
		}
	}

	pub async fn write(&mut self) {
		let base_url: String = match self.connection.as_mut() {
			Some(connection) => connection.base_url.clone(),
			None => panic!("No connection"),
		};

		let token: String = match self.connection.as_mut() {
			Some(connection) => connection.token.clone(),
			None => panic!("No connection"),
		};

		let org: String = match self.connection.as_mut() {
			Some(connection) => connection.org.clone(),
			None => panic!("No connection"),
		};

		let bucket: String = match self.connection.as_mut() {
			Some(connection) => connection.bucket.clone(),
			None => panic!("No connection"),
		};

		let precision = match self.connection.as_mut() {
			Some(connection) => match &connection.precision {
				Some(precision) => match precision {
					InfluxTimestampPrecision::Nanoseconds => "ns",
					InfluxTimestampPrecision::Microseconds => "us",
					InfluxTimestampPrecision::Milliseconds => "ms",
					InfluxTimestampPrecision::Seconds => "s",
				},
				None => "s",
			},
			None => panic!("No connection"),
		};

		let body = match self.connection.as_mut() {
			Some(_) => match self.measurement.as_mut() {
				Some(measurement) => measurement.build().await,
				None => panic!("No measurement"),
			},
			None => panic!("No connection"),
		};

		let write_url = format!("{}/api/v2/write?org={}&bucket={}&precision={}", base_url, org, bucket, precision);
		let _res = reqwest::Client::new().post(write_url).header("Content-Type", "text/plain; charset=utf-8").bearer_auth(token).body(body).send().await.unwrap();
		//println!("status: {}, {}", res.status(), res.text().await.unwrap());
	}

	pub async fn new_query(&mut self) {
		self.query = Some(FluxQuery::new());
	}

	pub async fn add_query_param(&mut self, param_name: &str, param_value: &str) {
		match self.query.as_mut().unwrap().params {
			Some(ref mut params) => {
				params.insert(param_name.to_string(), param_value.to_string());
			}
			None => {
				let mut params = HashMap::new();
				params.insert(param_name.to_string(), param_value.to_string());
				self.query.as_mut().unwrap().params = Some(params);
			}
		}
	}

	pub async fn select(&mut self, start: i64, interpolate: FluxInterpolation, filters: HashMap<String, InfluxField>) {
		//println!("select_two start: {}", start);
		let mut query = "import \"interpolate\"\n".to_string();
		query.push_str(&format!("from(bucket: \"{}\")\n", self.connection.as_mut().unwrap().bucket.clone()));
		query.push_str(&format!("\t|> range(start: {})\n", start));
		for (name, field) in filters {
			query.push_str(&format!("\t|> filter(fn: (r) => r[\"{}\"] == \"{}\")\n", name, field));
		}
		if interpolate != FluxInterpolation::None {
			query.push_str(format!("\t|> interpolate.linear(every: {})\n", interpolate).as_str());
		}
		query.push_str(" |> yield(name: \"last\")\n".to_string().as_str());
		//println!("Query: {}", query);
		self.query.as_mut().unwrap().query = Some(query);
	}

	pub async fn query(&mut self) -> String {
		let base_url: String = match self.connection.as_mut() {
			Some(connection) => connection.base_url.clone(),
			None => panic!("No connection"),
		};

		let token: String = match self.connection.as_mut() {
			Some(connection) => connection.token.clone(),
			None => panic!("No connection"),
		};

		let org: String = match self.connection.as_mut() {
			Some(connection) => connection.org.clone(),
			None => panic!("No connection"),
		};

		let query_url = format!("{}/api/v2/query?org={}", base_url, org);

		let mut query_body = HashMap::new();
		query_body.insert("query", self.query.as_mut().unwrap().query.clone().unwrap());
		query_body.insert("type", "flux".to_string());

		let body = serde_json::json!(query_body);

		let res = reqwest::Client::new().post(query_url).header("Content-Type", "application/json").bearer_auth(token).body(body.to_string()).send().await.unwrap();

		res.text().await.unwrap()
	}

	pub async fn delete(&mut self, item: FluxDelete) -> String {
		let connection = self.connection.as_mut().unwrap();
		let base_url: String = connection.base_url.clone();
		let token: String = connection.token.clone();
		let org: String = connection.org.clone();
		let bucket: String = connection.bucket.clone();

		let start = item.start.unwrap_or(0);

		let stop = match item.stop {
			Some(stop) => stop,
			None => chrono::Utc::now().timestamp(),
		};

		let predicate: String = match item.predicate {
			Some(predicate) => {
				let mut predicate_string = String::new();
				predicate.iter().enumerate().for_each(|(i, (key, value))| {
					if i == 0 {
						predicate_string.push_str(&format!("{}=\"{}\"", key, value));
					} else {
						predicate_string.push_str(&format!(" AND {}=\"{}\"", key, value));
					}
				});
				predicate_string
			}
			None => "".to_string(),
		};

		let start = chrono::DateTime::<chrono::Utc>::from_utc(chrono::NaiveDateTime::from_timestamp_opt(start, 0).unwrap(), chrono::Utc);
		let start = start.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
		let stop = chrono::DateTime::<chrono::Utc>::from_utc(chrono::NaiveDateTime::from_timestamp_opt(stop, 0).unwrap(), chrono::Utc);
		let stop = stop.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
		let mut query_body = HashMap::new();
		query_body.insert("start", start.to_string());
		query_body.insert("stop", stop.to_string());
		query_body.insert("predicate", predicate);

		let body = serde_json::json!(query_body);

		let query_url = format!("{}/api/v2/delete?org={}&bucket={}", base_url, org, bucket);
		let res = match reqwest::Client::new().post(query_url).header("Content-Type", "application/json").bearer_auth(token).body(body.to_string()).send().await {
			Ok(res) => res,
			Err(err) => {
				println!("Error: {}", err);
				println!("Connection: {:?}", connection);
				return "".to_string();
			}
		};

		res.text().await.unwrap()
	}
}
