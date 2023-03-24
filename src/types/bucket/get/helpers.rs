use crate::TimeSeriesMeasurement;
use axum::http::StatusCode;
use bigdecimal::BigDecimal;
use bigdecimal::ToPrimitive;
use sled::Tree;

pub async fn get_values_in_range(start: [u8; 8], end: [u8; 8], tree: Tree) -> Result<Vec<TimeSeriesMeasurement>, (StatusCode, String)> {
	let k_v = match tree.range(start..end).collect::<Result<Vec<(_, _)>, _>>() {
		Ok(values) => values,
		Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
	};

	let mut values = Vec::new();
	for (_, v) in k_v {
		let value = match serde_json::from_slice::<TimeSeriesMeasurement>(&v) {
			Ok(value) => value,
			Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
		};
		values.push(value);
	}

	Ok(values)
}

pub async fn extrapolate_start_for_empty_values(start: BigDecimal, start_of_range: [u8; 8], tree: Tree) -> Result<TimeSeriesMeasurement, (StatusCode, String)> {
	let s: Option<TimeSeriesMeasurement> = match tree.get(start_of_range) {
		Ok(v) => match v {
			Some(v) => {
				let value = String::from_utf8(v.to_vec()).unwrap();
				let value: TimeSeriesMeasurement = serde_json::from_str(value.as_str()).unwrap();
				Some(value)
			}
			None => None,
		},
		Err(_) => None,
	};

	if let Some(s) = s {
		Ok(s)
	} else {
		// extrapolate value for start
		let mut before = Vec::new();
		let before_last = match tree.range(..start_of_range).last() {
			Some(Ok(v)) => {
				let value = String::from_utf8(v.1.to_vec()).unwrap();
				let value: TimeSeriesMeasurement = serde_json::from_str(value.as_str()).unwrap();
				Some(value)
			}
			_ => None,
		};
		if before_last.is_some() {
			before.push(before_last.clone().unwrap());
		} else {
			return Err((StatusCode::BAD_REQUEST, "No values before start".to_string()));
		}
		let before_last = before_last.unwrap().timestamp.to_f64().unwrap().to_be_bytes();
		let before_next_last = match tree.range(..before_last).last() {
			Some(Ok(v)) => {
				let value = String::from_utf8(v.1.to_vec()).unwrap();
				let value: TimeSeriesMeasurement = serde_json::from_str(value.as_str()).unwrap();
				Some(value)
			}
			_ => None,
		};
		if before_next_last.is_some() {
			before.push(before_next_last.unwrap());
		} else {
			return Err((StatusCode::BAD_REQUEST, "No values before start".to_string()));
		}
		before.reverse();

		let new_value = ((before[1].value.clone() - before[0].value.clone()) / (before[1].timestamp.clone() - before[0].timestamp.clone())) * start.clone();
                let new_value = TimeSeriesMeasurement {
                        timestamp: start,
                        value: new_value,
                        tags: None,
                };
                Ok(new_value)
	}
}
