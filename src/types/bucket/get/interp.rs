use crate::types::bucket::StatusCode;
use bigdecimal::BigDecimal;

// linear linterpolation
// values: Vec<[x, y]>
// take: number of steps to take
pub async fn linear_interpolation(values: Vec<[BigDecimal; 2]>, start: BigDecimal, end: BigDecimal, take: BigDecimal) -> Result<Vec<[BigDecimal; 2]>, (StatusCode, String)> {
	let mut interpolated_values: Vec<[BigDecimal; 2]> = Vec::new();
	let step = (end.clone() - start.clone()) / take.clone();
	let mut current = start.clone();
	let mut i = 0;
	'outer: while current <= end {
		let value_i = values.get(i);
		if let Some(val) = value_i {
			if current == val[0] {
				interpolated_values.push(values[i].clone());
				i += 1;
				current += step.clone();
				continue 'outer;
			}
		}

		// get the closest two values to the current value
		let mut closest_value_index = 0;
		let mut distance = None;
		let mut j = 0;
		values.iter().for_each(|v| {
			let d = (v[0].clone() - current.clone()).abs();
			if distance.is_none() {
				distance = Some(d);
				closest_value_index = j;
			} else if let Some(dist) = distance.clone() {
				if d < dist {
					distance = Some(d);
					closest_value_index = j;
				}
			}
			j += 1;
		});

		// get the next closest value
		let mut next_closest_value_index = None;
		let mut distance = None;
		j = 0;
		values.iter().for_each(|v| {
			let d = (v[0].clone() - current.clone()).abs();
			if distance.is_none() {
				if j != closest_value_index {
					distance = Some(d);
					next_closest_value_index = Some(j);
				}
			} else if let Some(dist) = distance.clone() {
				if j != closest_value_index && d < dist {
					distance = Some(d);
					next_closest_value_index = Some(j);
				}
			}
			j += 1;
		});

		// if there is no next closest value, then use the closest value + 1
		// unless the closest value is the last value, then use the second to last value
		if next_closest_value_index.is_none() {
			if closest_value_index == values.len() - 1 {
				next_closest_value_index = Some(closest_value_index - 1);
			} else {
				next_closest_value_index = Some(closest_value_index + 1);
			}
		}

		let next_closest_value_index = next_closest_value_index.unwrap();

		// get the two values
		let mut vals: Vec<[BigDecimal; 2]> = vec![values[closest_value_index].clone(), values[next_closest_value_index].clone()];
		vals.sort_by(|a, b| a[0].clone().partial_cmp(&b[0].clone()).unwrap());

		let one = vals[0].clone();
		let two = vals[1].clone();
		let interp = linear_interp([one, two], current.clone()).await;
		interpolated_values.push(interp);

		current += step.clone();
	}

	Ok(interpolated_values)
}

#[tokio::test]
pub async fn test_linear_interpolation() {
	let values = vec![[BigDecimal::from(0_u8), BigDecimal::from(0_u8)], [BigDecimal::from(5_u8), BigDecimal::from(5_u8)], [BigDecimal::from(10_u8), BigDecimal::from(10_u8)]];
	let take = BigDecimal::from(10_u8);
	let start = BigDecimal::from(0_u8);
	let end = BigDecimal::from(10_u8);
	let interpolated_values = linear_interpolation(values, start, end, take).await.unwrap();
	assert_eq!(interpolated_values, vec![[BigDecimal::from(0_u8), BigDecimal::from(0_u8)], [BigDecimal::from(1_u8), BigDecimal::from(1_u8)], [BigDecimal::from(2_u8), BigDecimal::from(2_u8)], [BigDecimal::from(3_u8), BigDecimal::from(3_u8)], [BigDecimal::from(4_u8), BigDecimal::from(4_u8)], [BigDecimal::from(5_u8), BigDecimal::from(5_u8)], [BigDecimal::from(6_u8), BigDecimal::from(6_u8)], [BigDecimal::from(7_u8), BigDecimal::from(7_u8)], [BigDecimal::from(8_u8), BigDecimal::from(8_u8)], [BigDecimal::from(9_u8), BigDecimal::from(9_u8)], [BigDecimal::from(10_u8), BigDecimal::from(10_u8)]]);
}

// linear extrapolation
// takes two values and extrapolates a new value
pub async fn linear_interp(values: [[BigDecimal; 2]; 2], x: BigDecimal) -> [BigDecimal; 2] {
	let x1 = values[0][0].clone();
	let y1 = values[0][1].clone();
	let x2 = values[1][0].clone();
	let y2 = values[1][1].clone();

	// calculate slope
	let numerator = y2 - y1.clone();
	let denominator = x2 - x1.clone();
	let mut slope = BigDecimal::from(0_u8);
	if denominator != BigDecimal::from(0_u8) {
		slope = numerator / denominator;
	}
	// calculate y-intercept
	// y = y1 + m * (x - x1)
	let y_intercept = y1 + slope * (x.clone() - x1);

	[x, y_intercept]
}

#[tokio::test]
async fn test_linear_interp() {
	assert_eq!(linear_interp([[BigDecimal::from(4_u8), BigDecimal::from(2_u8)], [BigDecimal::from(5_u8), BigDecimal::from(7_u8)]], BigDecimal::from(6_u8)).await, [BigDecimal::from(6_u8), BigDecimal::from(12_u8)]);
	assert_eq!(linear_interp([[BigDecimal::from(0_u8), BigDecimal::from(0_u8)], [BigDecimal::from(1_u8), BigDecimal::from(1_u8)]], BigDecimal::from(-1_i8)).await, [BigDecimal::from(-1_i8), BigDecimal::from(-1_i8)]);
}
