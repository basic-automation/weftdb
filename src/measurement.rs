use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

///
/// # Measurement
/// ## Properties
/// * source: source of the measurement (e.g. "thorchain")
/// * numerator_asset: numerator asset of the measurement (e.g. "rune")
/// * denominator_asset: denominator asset of the measurement (e.g. "usd")
/// * uuid: uuid of the measurement
/// * timestamp: timestamp of the measurement was taken
/// * ratio: ratio of the measurement (e.g. numerator_asset_value/denominator_asset_value)
/// * location: same as the timestamp but mutable by processing the measurement
/// * amplitude: same as the ratio but mutable by processing the measurement
/// * positive_distance: todo!()
/// * negative_distance: todo!()
/// * trend_vectors: todo!()

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Measurement {
	pub source: String,
	pub numerator_asset: String,
	pub denominator_asset: String,
	pub uuid: Uuid,
	pub timestamp: BigDecimal,
	pub ratio: BigDecimal,
	pub location: Option<BigDecimal>,
	pub amplitude: Option<BigDecimal>,
	pub positive_distance: Option<BigDecimal>,
	pub negative_distance: Option<BigDecimal>,
	pub trend_vectors: Option<Vec<BigDecimal>>,
}

impl Measurement {
	pub fn new(source: &str, numerator_asset: &str, denominator_asset: &str, uuid: Uuid, timestamp: BigDecimal, ratio: BigDecimal) -> Self {
		Self {
			source: source.to_string(),
			numerator_asset: numerator_asset.to_string(),
			denominator_asset: denominator_asset.to_string(),
			uuid,
			timestamp,
			ratio,
			location: None,
			amplitude: None,
			positive_distance: None,
			negative_distance: None,
			trend_vectors: None,
		}
	}
}



