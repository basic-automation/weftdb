use super::Occurrence;
use crate::{Interpolation, VariabilityType};
use bigdecimal::BigDecimal;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::{serde_as, DisplayFromStr};
use std::collections::HashMap;
use urlencoding::encode;
use uuid::Uuid;

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pattern {
	pub id: Uuid,
	#[serde_as(as = "HashMap<DisplayFromStr, _>")]
	pub occurrences: HashMap<BigUint, Occurrence>,
	#[serde_as(as = "HashMap<DisplayFromStr, _>")]
	pub locations: HashMap<BigUint, BigDecimal>,
	#[serde_as(as = "HashMap<DisplayFromStr, _>")]
	pub amplitudes: HashMap<BigUint, BigDecimal>,
	pub merged_patterns: Option<Vec<Uuid>>,
}

impl Pattern {
	pub fn merge_occurrences(&mut self, pattern: &Pattern) {
		for (_, occurrence) in pattern.occurrences.iter() {
			self.occurrences.insert(self.occurrences.len().into(), occurrence.clone());
		}
		let mut merged_patterns = self.merged_patterns.clone().unwrap_or_default();
		merged_patterns.push(pattern.id);
		self.merged_patterns = Some(merged_patterns);
	}

	pub async fn enforce_steps(&mut self, step_spacing: f64, interpolation: Interpolation) -> Self {
		// create bucket
		let client = reqwest::Client::new();
		let encoded_id = encode(&self.id.to_string()).to_string();
		let url = format!("http://127.0.0.1:8515/bucket?name={}&type=timeseries", format_args!("{}_temp", encoded_id));
		let res = client.post(&url).send().await.unwrap();

		let bucket = res.json::<Value>().await.unwrap();
		let bucket = Bucket { uuid: Uuid::parse_str(bucket["uuid"].as_str().unwrap()).unwrap(), name: bucket["name"].as_str().unwrap().to_string(), tags: HashMap::new(), type_: "TimeSeries".to_string() };

		// insert data
		for location in self.locations.iter() {
			let url = format!("http://127.0.0.1:8515/bucket/{}?key={}&value={}", bucket.name, location.1, self.amplitudes[location.0]);
			client.post(&url).send().await.unwrap();
		}

		// get interpolated data
		let interpolation = match interpolation {
			Interpolation::Linear => "linear",
		};

		let url = format!("http://127.0.0.1:8515/bucket/{}?interpolation={}&steps={}", bucket.name, interpolation, step_spacing);
		let res = client.get(&url).send().await.unwrap();
		match res.json::<Value>().await {
			Ok(data) => {
				let mut locations: HashMap<BigUint, BigDecimal> = HashMap::new();
				let mut amplitudes: HashMap<BigUint, BigDecimal> = HashMap::new();
				let mut data: Vec<Measurement> = serde_json::from_value(data["value"].clone()).unwrap();
				data.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
				for measurement in data {
					locations.insert(BigUint::from(locations.len()), measurement.timestamp);
					amplitudes.insert(BigUint::from(amplitudes.len()), measurement.value);
				}
				self.locations = locations;
				self.amplitudes = amplitudes;
			}
			Err(e) => println!("Error: {}", e),
		}

		// delete bucket
		let url = format!("http://127.0.0.1:8515/bucket/{}", bucket.name);
		client.delete(&url).send().await.unwrap();

		self.clone()
	}

	pub async fn static_variability(&self, compare_pattern: &Pattern, type_: &VariabilityType) -> BigDecimal {
		let mut variability: Vec<BigDecimal> = Vec::new();
		if self.amplitudes.len() == compare_pattern.amplitudes.len() {
			for (index, amplitude) in self.amplitudes.iter() {
				let compare_amplitude = compare_pattern.amplitudes.get(index).unwrap();
				let difference = amplitude - compare_amplitude;
				variability.push(difference.abs());
			}
		}

		match type_ {
			VariabilityType::MaxVariability => {
				variability.sort();
				variability.last().unwrap().clone()
			}
			VariabilityType::AverageVariability => {
				let mut sum = BigDecimal::from(0);
				for value in &variability {
					sum += value;
				}
				sum / BigDecimal::from(variability.len() as u64)
			}
			VariabilityType::SumVariability => {
				let mut sum = BigDecimal::from(0);
				for value in variability {
					sum += value;
				}
				sum
			}
		}
	}

	///If Max-Vairability type is set, the two patterns will be deemed the same if the difference between the absolute value of any of the congruent Amplitudes is less than the AbsoluteStaticVariability.Value.
	/// If Average-Variability type is set, the two patterns will be deemed the same if the average of all of the differences between the absolute value of the congruent Amplitudes is less than AbsoluteStaticVariability.Value.
	/// If Sum-Variability type is set, the two patterns will be deemed the same if the sum of all of the differences between the absolute value of the congruent Amplitudes is less than AbsoluteStaticVariability.Value.
	pub async fn absolute_static_variability(&self, compare_pattern: &Pattern, type_: &VariabilityType) -> BigDecimal {
		let mut variability: Vec<BigDecimal> = Vec::new();
		if self.amplitudes.len() == compare_pattern.amplitudes.len() {
			for (index, amplitude) in self.amplitudes.iter() {
				let compare_amplitude = compare_pattern.amplitudes.get(index).unwrap();
				let difference = amplitude.abs() - compare_amplitude.abs();
				variability.push(difference.abs());
			}
		}

		match type_ {
			VariabilityType::MaxVariability => {
				variability.sort();
				variability.last().unwrap().clone()
			}
			VariabilityType::AverageVariability => {
				let mut sum = BigDecimal::from(0);
				for value in &variability {
					sum += value;
				}
				sum / BigDecimal::from(variability.len() as u64)
			}
			VariabilityType::SumVariability => {
				let mut sum = BigDecimal::from(0);
				for value in variability {
					sum += value;
				}
				sum
			}
		}
	}

	pub async fn percentage_variability(&self, compare_pattern: &Pattern, type_: &VariabilityType) -> BigDecimal {
		let mut variability: Vec<BigDecimal> = Vec::new();
		if self.amplitudes.len() == compare_pattern.amplitudes.len() {
			for (index, amplitude) in self.amplitudes.iter() {
				let compare_amplitude = compare_pattern.amplitudes.get(index).unwrap();
				let numerator = (amplitude - compare_amplitude).abs();
				let denominator = (amplitude + compare_amplitude) / BigDecimal::from(2);
				if denominator == BigDecimal::from(0) {
					variability.push(BigDecimal::from(0));
					continue;
				}
				let decimal_percentage = numerator / denominator;
				variability.push(decimal_percentage);
			}
		}

		match type_ {
			VariabilityType::MaxVariability => {
				variability.sort();
				variability.last().unwrap().clone()
			}
			VariabilityType::AverageVariability => {
				let mut sum = BigDecimal::from(0);
				for value in &variability {
					sum += value;
				}
				sum / BigDecimal::from(variability.len() as u64)
			}
			VariabilityType::SumVariability => {
				let mut sum = BigDecimal::from(0);
				for value in variability {
					sum += value;
				}
				sum
			}
		}
	}

	pub async fn absolute_percentage_variability(&self, compare_pattern: &Pattern, type_: &VariabilityType) -> BigDecimal {
		let mut variability: Vec<BigDecimal> = Vec::new();
		if self.amplitudes.len() == compare_pattern.amplitudes.len() {
			for (index, amplitude) in self.amplitudes.iter() {
				let compare_amplitude = compare_pattern.amplitudes.get(index).unwrap();
				let numerator = (amplitude.abs() - compare_amplitude.abs()).abs();
				let denominator = (amplitude.abs() + compare_amplitude.abs()) / BigDecimal::from(2);
				if denominator == BigDecimal::from(0) {
					variability.push(BigDecimal::from(0));
					continue;
				}
				let decimal_percentage = numerator / denominator;
				variability.push(decimal_percentage);
			}
		}

		match type_ {
			VariabilityType::MaxVariability => {
				variability.sort();
				variability.last().unwrap().clone()
			}
			VariabilityType::AverageVariability => {
				let mut sum = BigDecimal::from(0);
				for value in &variability {
					sum += value;
				}
				sum / BigDecimal::from(variability.len() as u64)
			}
			VariabilityType::SumVariability => {
				let mut sum = BigDecimal::from(0);
				for value in variability {
					sum += value;
				}
				sum
			}
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bucket {
	name: String,
	tags: HashMap<String, String>,
	#[serde(rename = "type")]
	type_: String,
	uuid: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Measurement {
	tags: Option<HashMap<String, String>>,
	timestamp: BigDecimal,
	value: BigDecimal,
}
