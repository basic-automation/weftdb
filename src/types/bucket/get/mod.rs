use crate::{
	types::bucket::{BucketParams, BucketValue, ObjectValue},
	Interpolation, TimeSeriesMeasurement,
};
use axum::http::StatusCode;
use bigdecimal::{BigDecimal, ToPrimitive};
pub use interp::*;
use sled::Tree;
use std::collections::HashMap;
use std::str::FromStr;
use core::cmp::Ordering;

mod interp;

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

pub async fn get_timeseries_tree(tree: Tree) -> Result<Vec<TimeSeriesMeasurement>, (StatusCode, String)> {
        let k_v = match tree.iter().collect::<Result<Vec<(_, _)>, _>>() {
                Ok(values) => values,
                Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        };

        let mut values: Vec<TimeSeriesMeasurement> = Vec::new();
        for v in k_v {
                let value = match String::from_utf8(v.1.to_vec()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
                let value: TimeSeriesMeasurement = match serde_json::from_str(value.as_str()) {
                        Ok(value) => value,
                        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                };
                values.push(value);
        }
        // sort values by timestamp
        values.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        Ok(values)
}

pub async fn get_timeseries_bucket(tree: Tree, params: BucketParams) -> Result<Vec<BucketValue>, (StatusCode, String)> {
	let mut res = Vec::new();

	let range_start = match params.range.clone() {
		Some(range) => match BigDecimal::from_str(&range[0]) {
			Ok(value) => Some(value),
			Err(_) => None,
		},
		None => None,
	};

	let range_end = match params.range {
		Some(range) => match BigDecimal::from_str(&range[1]) {
			Ok(value) => Some(value),
			Err(_) => None,
		},
		None => None,
	};

	let range = match (range_start.clone(), range_end.clone()) {
		(Some(range_start), Some(range_end)) => {
                        let mut range: Vec<TimeSeriesMeasurement> = Vec::new();
                        if range_start < BigDecimal::from(0_u8) || range_end < BigDecimal::from(0_u8) {
                                let values = get_timeseries_tree(tree.clone()).await?;
                                for value in values {
                                        if value.timestamp >= range_start && value.timestamp <= range_end {
                                                range.push(value);
                                        }
                                }
                        } else {
                                let r = tree.range(range_start.to_f64().unwrap().to_be_bytes()..range_end.to_f64().unwrap().to_be_bytes()).collect::<Result<Vec<(_, _)>, _>>().unwrap();

        			range = r
        				.into_iter()
        				.map(|v| {
        					let value = match String::from_utf8(v.1.to_vec()) {
        						Ok(value) => value,
        						Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        					};
        					let value: TimeSeriesMeasurement = match serde_json::from_str(value.as_str()) {
        						Ok(value) => value,
        						Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        					};
        					Ok(value)
        				})
        				.collect::<Result<Vec<TimeSeriesMeasurement>, _>>()?;
                        }
			

			let end = match tree.get(range_end.to_f64().unwrap().to_be_bytes()) {
				Ok(end) => match end {
					Some(end) => match String::from_utf8(end.to_vec()) {
						Ok(end) => match serde_json::from_str::<TimeSeriesMeasurement>(end.as_str()) {
							Ok(end) => Some(end),
							Err(_) => None,
						},
						Err(_) => None,
					},
					None => None,
				},
				Err(_) => None,
			};
			if let Some(end) = end {
				range.push(end);
			}
			range
		}
		_ => match tree.iter().collect::<Result<Vec<(_, _)>, _>>() {
			Ok(values) => values
				.into_iter()
				.map(|v| {
					let value = match String::from_utf8(v.1.to_vec()) {
						Ok(value) => value,
						Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
					};
					let value: TimeSeriesMeasurement = match serde_json::from_str(value.as_str()) {
						Ok(value) => value,
						Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
					};
					Ok(value)
				})
				.collect::<Result<Vec<TimeSeriesMeasurement>, _>>()?,
			Err(_) => Vec::new(),
		},
	};

	if range.len() == 1 {
		for v in &range {
			res.push(BucketValue::TimeSeries(v.clone()));
		}

                if let Some(_) = params.interpolation {
                        let take = params.take.unwrap_or(BigDecimal::from(60_u16));
                        let step = (range_end.clone().unwrap() - range_start.clone().unwrap()) / take.clone();
                        let mut i = range_start.unwrap();
                        
                        while i < range_end.clone().unwrap() {
                                if i == range[0].timestamp {
                                        i = i + step.clone();
                                        continue;
                                }
                                let m = TimeSeriesMeasurement {
                                        timestamp: i.clone(),
                                        value: range[0].clone().value,
                                        tags: None,
                                };
                                res.push(BucketValue::TimeSeries(m));

                                i = i + step.clone();
                        }

                        let m = TimeSeriesMeasurement {
                                timestamp: range_end.clone().unwrap().clone(),  
                                value: range[0].value.clone(),
                                tags: None,
                        };
                        res.push(BucketValue::TimeSeries(m));

                        // sort res by timestamp
                        res.sort_by(|a, b| {
                                match a {
                                        BucketValue::TimeSeries(a) => {
                                                match b {
                                                        BucketValue::TimeSeries(b) => a.timestamp.cmp(&b.timestamp),
                                                        _ => Ordering::Less,
                                                }
                                        }
                                        _ => Ordering::Less,
                                }
                        });
                }
		return Ok(res);
	}

	let range_start = match range_start {
		Some(r_s) => r_s,
		None => range[0].timestamp.clone(),
	};

	let range_end = match range_end {
		Some(r_e) => r_e,
		None => range[range.len() - 1].timestamp.clone(),
	};

	let mut preserve_tags: Vec<(BigDecimal, Option<HashMap<String, String>>)> = Vec::new();
	for v in &range {
		let tags = v.tags.clone();
		preserve_tags.push((v.timestamp.clone(), tags.clone()));
	}

	if let Some(interpolation) = params.interpolation {
		if interpolation == Interpolation::Linear {
			let mut new_values: Vec<[BigDecimal; 2]> = Vec::new();
			for v in range {
				new_values.push([v.timestamp.clone(), v.value.clone()]);
			}

			let new_values = interp::linear_interpolation(new_values, range_start, range_end, params.take.unwrap_or(BigDecimal::from(60_u16))).await.unwrap();
			for value in new_values {
				let tags = match preserve_tags.iter().find(|x| x.0 == value[0]) {
					Some(tags) => tags.clone().1,
					None => {
						let mut tags: HashMap<String, String> = HashMap::new();
						tags.insert("interpolated".to_string(), "true".to_string());
						Some(tags.clone())
					}
				};

				res.push(BucketValue::TimeSeries(TimeSeriesMeasurement { timestamp: value[0].clone(), value: value[1].clone(), tags: tags.clone() }));
			}
		}
	} else {
		for v in range {
			res.push(BucketValue::TimeSeries(TimeSeriesMeasurement { timestamp: v.timestamp.clone(), value: v.value.clone(), tags: v.tags.clone() }));
		}
	}

	Ok(res)
}
