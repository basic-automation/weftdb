use crate::types::{
	bucket::{BucketParams, BucketValue, ObjectValue, TimeSeriesMeasurement},
	interpolation,
};
use bigdecimal::{BigDecimal, FromPrimitive};
use sled::Tree;

pub async fn get_object_bucket(tree: Tree) -> Result<Vec<BucketValue>, String> {
	let k_v = match tree.iter().collect::<Result<Vec<(_, _)>, _>>() {
		Ok(values) => values,
		Err(e) => return Err(e.to_string()),
	};
	let values: Vec<BucketValue> = k_v
		.iter()
		.map(|v| {
			let value = String::from_utf8(v.1.to_vec()).unwrap();
			let mut value: ObjectValue = serde_json::from_str(value.as_str()).unwrap();
			value.value = serde_json::from_str(value.value.as_str().unwrap()).unwrap();
			BucketValue::Object(value)
		})
		.collect();
	Ok(values)
}

pub async fn get_timeseries_bucket(tree: Tree, params: BucketParams) -> Result<Vec<BucketValue>, String> {
	let k_v = match tree.iter().collect::<Result<Vec<_>, _>>() {
		Ok(values) => values,
		Err(e) => return Err(e.to_string()),
	};
	let values: Vec<BucketValue> = k_v
		.iter()
		.map(|v| {
			let value = String::from_utf8(v.1.to_vec()).unwrap();
			let value: TimeSeriesMeasurement = serde_json::from_str(value.as_str()).unwrap();
			BucketValue::TimeSeries(value)
		})
		.collect();

	let mut values = match params.range {
		Some(range) => {
			let start = range[0];
			let end = range[1];
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

	let steps = params.steps.unwrap_or(60);

	if let Some(interpolation) = params.interpolation {
		values = interpolate(values, steps, interpolation).await
	}

	Ok(values)
}

async fn interpolate(values: Vec<BucketValue>, steps: u64, interpolation: interpolation::Interpolation) -> Vec<BucketValue> {
	match interpolation {
		interpolation::Interpolation::Linear => linear_interpolation(values, steps).await.unwrap(),
	}
}

// linear linterpolate from values.first() to values.last() every steps seconds
pub async fn linear_interpolation(values: Vec<BucketValue>, steps: u64) -> Result<Vec<BucketValue>, String> {
	let mut interpolated_values: Vec<BucketValue> = Vec::new();
	let values: Vec<TimeSeriesMeasurement> = values
		.iter()
		.filter_map(|v| match v {
			BucketValue::TimeSeries(value) => Some(value.clone()),
			_ => None,
		})
		.collect();

	let mut new_timestamps = Vec::new();
	new_timestamps.push(values.first().unwrap().timestamp);
	let mut i = 1;
	while new_timestamps.last().unwrap() < &values.last().unwrap().timestamp {
		new_timestamps.push(values.first().unwrap().timestamp + (steps as i64 * i));
		i += 1;
	}
	// sort new_timestamps
	new_timestamps.sort();
	new_timestamps.pop();
	new_timestamps.push(values.last().unwrap().timestamp);

	let mut i = 0;
	let mut j = 0;
	while i < new_timestamps.len() {
		let mut new_value = TimeSeriesMeasurement { timestamp: new_timestamps[i], value: BigDecimal::from_f64(0.0).unwrap(), tags: None };
		while j < values.len() - 1 {
			if new_timestamps[i] >= values[j].timestamp && new_timestamps[i] <= values[j + 1].timestamp {
				let slope = (values[j + 1].value.clone() - values[j].value.clone()) / BigDecimal::from_f64((values[j + 1].timestamp - values[j].timestamp) as f64).unwrap();
				let y_intercept = values[j].value.clone() - (slope.clone() * BigDecimal::from_f64(values[j].timestamp as f64).unwrap());
				new_value.value = (slope.clone() * BigDecimal::from_f64(new_timestamps[i] as f64).unwrap()) + y_intercept;
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
