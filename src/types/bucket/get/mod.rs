use crate::types::{
	bucket::{BucketParams, BucketValue, ObjectValue, TimeSeriesMeasurement},
	interpolation,
};
use axum::http::StatusCode;
use bigdecimal::BigDecimal;
use sled::Tree;
use std::str::FromStr;

pub async fn get_object_bucket(tree: Tree) -> Result<Vec<BucketValue>, (StatusCode, String)> {
	let k_v = match tree.iter().collect::<Result<Vec<(_, _)>, _>>() {
		Ok(values) => values,
		Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
	};

        let mut values: Vec<BucketValue> = Vec::new();
        for v in k_v {
                let value = match String::from_utf8(v.1.to_vec()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
                let mut value: ObjectValue = match serde_json::from_str(value.as_str()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
                let val = match value.value.as_str() {
                        Some(val) => val,
                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Invalid value".to_string())),
                };
                value.value = match serde_json::from_str(val) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
                values.push(BucketValue::Object(value));
        }
	Ok(values)
}

pub async fn get_timeseries_bucket(tree: Tree, params: BucketParams) -> Result<Vec<BucketValue>, (StatusCode, String)> {
	let k_v = match tree.iter().collect::<Result<Vec<_>, _>>() {
		Ok(values) => values,
		Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
	};

        let mut values: Vec<BucketValue> = Vec::new();
        for i in 0..k_v.len() {
                let item = match k_v.get(i) {
                        Some(item) => item,
                        None => break,
                };
                let (_, value) = item;

                let value = match String::from_utf8(value.to_vec()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };

                let value: TimeSeriesMeasurement = match serde_json::from_str(value.as_str()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
                values.push(BucketValue::TimeSeries(value));
        }

	let mut values = match params.range {
		Some(range) => {
			let start = match BigDecimal::from_str(&range[0]) {
				Ok(start) => start,
				Err(_) => return Err((StatusCode::BAD_REQUEST, "Invalid range[0]".to_string())),
			};
			let end = match BigDecimal::from_str(&range[1]) {
				Ok(end) => end,
				Err(_) => return Err((StatusCode::BAD_REQUEST, "Invalid range[1]".to_string())),
			};
			values
				.iter()
				.filter(|v| match v {
					BucketValue::TimeSeries(value) => value.timestamp >= start && value.timestamp <= end,
					_ => false,
				})
				.cloned()
				.collect()
		}
		None => values,
	};

	let steps = params.steps.unwrap_or(BigDecimal::from(60_u32));

	if let Some(interpolation) = params.interpolation {
		values = match interpolate(values, steps, interpolation).await {
                        Ok(values) => values,
                        Err(e) => return Err(e),
                };
	}

	Ok(values)
}

async fn interpolate(values: Vec<BucketValue>, steps: BigDecimal, interpolation: interpolation::Interpolation) -> Result<Vec<BucketValue>, (StatusCode, String)> {
	match interpolation {
		interpolation::Interpolation::Linear => match linear_interpolation(values, steps).await {
			Ok(bv) => Ok(bv),
			Err(e) => Err(e),
		},
	}
}

// linear linterpolate from values.first() to values.last() every steps seconds
pub async fn linear_interpolation(values: Vec<BucketValue>, steps: BigDecimal) -> Result<Vec<BucketValue>, (StatusCode, String)> {
	let mut interpolated_values: Vec<BucketValue> = Vec::new();
	let values: Vec<TimeSeriesMeasurement> = values
		.iter()
		.filter_map(|v| match v {
			BucketValue::TimeSeries(value) => Some(value.clone()),
			_ => None,
		})
		.collect();

	let mut new_timestamps = Vec::new();
	let first_value = match values.first() {
		Some(v) => v.clone(),
		None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No first value found.".to_string())),
	};
	new_timestamps.push(first_value.timestamp);
	let mut i = 1;
        let mut last_new_timestamp = match new_timestamps.last() {
                Some(v) => v.clone(),
                None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No last new timestamp found.".to_string())),
        };
        let mut last_value_timestamp = match values.last() {
                Some(v) => v.timestamp.clone(),
                None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No last value timestamp found.".to_string())),
        };
	while last_new_timestamp < last_value_timestamp {
                let first_value_timestamp = match values.first() {
                        Some(v) => v.timestamp.clone(),
                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No first value timestamp found.".to_string())),
                };
		new_timestamps.push(first_value_timestamp + (steps.clone() * BigDecimal::from(i)));

                last_new_timestamp = match new_timestamps.last() {
                        Some(v) => v.clone(),
                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No last new timestamp found.".to_string())),
                };
                last_value_timestamp = match values.last() {
                        Some(v) => v.timestamp.clone(),
                        None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No last value timestamp found.".to_string())),
                };

		i += 1;
        }

        // add last value timestamp
        last_value_timestamp = match values.last() {
                Some(v) => v.timestamp.clone(),
                None => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Iterpolation Error: No last value timestamp found.".to_string())),
        };

	// sort new_timestamps
	new_timestamps.sort();
	new_timestamps.pop();
	new_timestamps.push(last_value_timestamp);

	let mut i = 0;
	let mut j = 0;
	while i < new_timestamps.len() {
		let mut new_value = TimeSeriesMeasurement { timestamp: new_timestamps[i].clone(), value: BigDecimal::from(0_u8), tags: None };
		while j < values.len() - 1 {
			if new_timestamps[i] >= values[j].timestamp && new_timestamps[i] <= values[j + 1].timestamp {
				let slope = (values[j + 1].value.clone() - values[j].value.clone()) / (values[j + 1].timestamp.clone() - values[j].timestamp.clone());
				let y_intercept = values[j].value.clone() - (slope.clone() * values[j].timestamp.clone());
				new_value.value = (slope.clone() * new_timestamps[i].clone()) + y_intercept;
				new_value.tags = values[j].tags.clone();
				interpolated_values.push(BucketValue::TimeSeries(new_value));
				break;
			}
			j += 1;
		}
		i += 1;
	}
	Ok(interpolated_values)
}
