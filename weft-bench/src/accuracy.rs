//! Reconstruction-accuracy metrics over a known ground truth.
//!
//! Latency answers "how fast"; this module answers "how right". For the seeded
//! synthetic profile the underlying signal is a known analytic function (see
//! [`InterpolationProfile::clean_signal_at`]), so a reconstruction's output grid
//! can be scored against the *true* shape it was meant to recover. This is the
//! quality axis the roadmap repeatedly demands — fair-protocol Phase 1.2
//! ("benchmark interpolation **quality** ... as well as speed") and Phase 6.4
//! (RMSE / MAE / max-error / bias) — and it lets the three reconstruction methods
//! (WeftDB spline, linear, forward-fill / LOCF) be compared on accuracy, not only
//! speed.
//!
//! ## On `f64`
//!
//! [`AccuracyMetrics`] are *error statistics* (squares, a square root, means)
//! summarizing how far a reconstruction lands from an analytic `f64` ground
//! truth — a reporting quantity, not a stored or hot-path measurement value.
//! Computing them in `f64` does not violate the no-silent-downcast rule (which
//! governs the logical/API measurement type): nothing here is persisted or
//! interpolated. Each predicted `BigDecimal` is converted exactly once, only to
//! fold it into the summary.

use bigdecimal::ToPrimitive;
use serde::{Deserialize, Serialize};
use splimes::{generate_target_times, Point};

use crate::profile::InterpolationProfile;

/// Why an accuracy measurement could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccuracyError {
	/// Predicted and truth sequences had different lengths, so they cannot be
	/// aligned point-for-point.
	LengthMismatch {
		/// Number of predicted (reconstructed) values.
		predicted: usize,
		/// Number of ground-truth values.
		truth: usize,
	},
	/// There were no aligned points to score.
	Empty,
	/// The predicted value at `index` was not a finite number.
	NonFinite {
		/// Position of the offending predicted value.
		index: usize,
	},
	/// The profile has no analytic ground truth (a line-protocol source).
	NoGroundTruth,
}

impl std::fmt::Display for AccuracyError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::LengthMismatch { predicted, truth } => write!(f, "cannot align {predicted} predicted values with {truth} ground-truth values"),
			Self::Empty => write!(f, "no aligned points to score"),
			Self::NonFinite { index } => write!(f, "predicted value at index {index} is not finite"),
			Self::NoGroundTruth => write!(f, "profile has no analytic ground truth (line-protocol source)"),
		}
	}
}

impl std::error::Error for AccuracyError {}

/// Accuracy of a reconstruction against a known ground truth.
///
/// All four error statistics are in the same units as the measured values. By
/// construction they satisfy `max_abs_error >= rmse >= mae >= 0` and
/// `|bias| <= mae`, which the unit tests assert as invariants.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AccuracyMetrics {
	/// Number of aligned points scored.
	pub count: usize,
	/// Root-mean-square error — penalizes large deviations.
	pub rmse: f64,
	/// Mean absolute error — average deviation magnitude.
	pub mae: f64,
	/// Largest single absolute deviation (worst-case).
	pub max_abs_error: f64,
	/// Mean *signed* error (`predicted - truth`): systematic over/under-shoot.
	pub bias: f64,
}

impl AccuracyMetrics {
	/// Score aligned `predicted` reconstruction values against `truth` ground-truth
	/// values: the i-th predicted point is compared to the i-th truth value, so the
	/// caller must evaluate truth at the same grid instants the adapter produced
	/// (see [`synthetic_ground_truth`]).
	///
	/// # Errors
	///
	/// [`AccuracyError::LengthMismatch`] if the sequences differ in length,
	/// [`AccuracyError::Empty`] if there is nothing to score, and
	/// [`AccuracyError::NonFinite`] if a predicted value is not a finite number.
	pub fn from_aligned(predicted: &[Point], truth: &[f64]) -> Result<Self, AccuracyError> {
		if predicted.len() != truth.len() {
			return Err(AccuracyError::LengthMismatch { predicted: predicted.len(), truth: truth.len() });
		}
		if predicted.is_empty() {
			return Err(AccuracyError::Empty);
		}

		let mut sum_sq = 0.0_f64;
		let mut sum_abs = 0.0_f64;
		let mut sum_signed = 0.0_f64;
		let mut max_abs = 0.0_f64;
		for (index, (p, &t)) in predicted.iter().zip(truth.iter()).enumerate() {
			// A `BigDecimal` is never NaN/inf, but an extreme magnitude can overflow
			// the `f64` conversion; treat any non-finite (or unrepresentable) value
			// as a hard error rather than letting it poison the statistics.
			let pv = p.value.to_f64().filter(|v| v.is_finite()).ok_or(AccuracyError::NonFinite { index })?;
			let err = pv - t;
			sum_sq = err.mul_add(err, sum_sq);
			sum_abs += err.abs();
			sum_signed += err;
			if err.abs() > max_abs {
				max_abs = err.abs();
			}
		}

		#[allow(clippy::cast_precision_loss)]
		let n = predicted.len() as f64;
		Ok(Self { count: predicted.len(), rmse: (sum_sq / n).sqrt(), mae: sum_abs / n, max_abs_error: max_abs, bias: sum_signed / n })
	}
}

/// Build the synthetic profile's noise-free ground-truth grid.
///
/// Evaluates the analytic clean signal at every instant of the reconstruction
/// grid the adapter is asked to produce
/// (`generate_target_times(start, end, resolution)`), so the result aligns
/// one-for-one with a passing adapter's output.
///
/// # Errors
///
/// Returns [`AccuracyError::NoGroundTruth`] for a line-protocol profile, which has
/// no known true signal to score against.
pub fn synthetic_ground_truth(profile: &InterpolationProfile) -> Result<Vec<f64>, AccuracyError> {
	let targets = generate_target_times(profile.start(), profile.end(), profile.resolution);
	targets.iter().map(|t| profile.clean_signal_at(*t).ok_or(AccuracyError::NoGroundTruth)).collect()
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use chrono::{TimeZone, Utc};
	use splimes::{Resolution, Spline};

	use super::*;
	use crate::line_protocol::TimestampPrecision;

	/// Build `Point`s at arbitrary (here irrelevant) instants carrying the given
	/// values; `from_aligned` only reads the values, positionally.
	fn points_with_values(values: &[f64]) -> Vec<Point> {
		values.iter().enumerate().map(|(i, &v)| Point { timestamp: Utc.timestamp_opt(i64::try_from(i).unwrap(), 0).single().unwrap(), value: BigDecimal::try_from(v).unwrap() }).collect()
	}

	#[test]
	fn a_perfect_reconstruction_scores_zero_on_every_metric() {
		let truth = [10.0, 20.0, 35.5, 80.0];
		let predicted = points_with_values(&truth);
		let m = AccuracyMetrics::from_aligned(&predicted, &truth).expect("scores");
		assert_eq!(m.count, 4);
		assert!(m.rmse.abs() < 1e-9, "rmse {} should be ~0", m.rmse);
		assert!(m.mae.abs() < 1e-9, "mae {} should be ~0", m.mae);
		assert!(m.max_abs_error.abs() < 1e-9, "max {} should be ~0", m.max_abs_error);
		assert!(m.bias.abs() < 1e-9, "bias {} should be ~0", m.bias);
	}

	#[test]
	fn a_constant_overshoot_surfaces_as_bias_equal_to_the_offset() {
		let truth = [10.0, 20.0, 30.0, 40.0];
		// Every prediction is exactly +5 above truth.
		let predicted = points_with_values(&[15.0, 25.0, 35.0, 45.0]);
		let m = AccuracyMetrics::from_aligned(&predicted, &truth).expect("scores");
		assert!((m.bias - 5.0).abs() < 1e-9, "bias must equal the +5 offset, got {}", m.bias);
		assert!((m.mae - 5.0).abs() < 1e-9, "mae must equal 5, got {}", m.mae);
		assert!((m.rmse - 5.0).abs() < 1e-9, "rmse must equal 5, got {}", m.rmse);
		assert!((m.max_abs_error - 5.0).abs() < 1e-9, "max must equal 5, got {}", m.max_abs_error);
	}

	#[test]
	fn a_symmetric_error_cancels_in_bias_but_not_in_magnitude_metrics() {
		let truth = [10.0, 20.0];
		// +4 then -4: signed errors cancel (bias 0) but magnitudes do not.
		let predicted = points_with_values(&[14.0, 16.0]);
		let m = AccuracyMetrics::from_aligned(&predicted, &truth).expect("scores");
		assert!(m.bias.abs() < 1e-9, "opposite errors must cancel in bias, got {}", m.bias);
		assert!((m.mae - 4.0).abs() < 1e-9, "mae must be 4, got {}", m.mae);
		assert!((m.rmse - 4.0).abs() < 1e-9, "rmse must be 4, got {}", m.rmse);
		assert!((m.max_abs_error - 4.0).abs() < 1e-9, "max must be 4, got {}", m.max_abs_error);
	}

	#[test]
	fn metric_invariants_hold_on_a_mixed_error_profile() {
		let truth = [0.0, 0.0, 0.0, 0.0];
		let predicted = points_with_values(&[1.0, -3.0, 2.0, -2.0]);
		let m = AccuracyMetrics::from_aligned(&predicted, &truth).expect("scores");
		// max >= rmse >= mae >= 0, and |bias| <= mae, always.
		assert!(m.max_abs_error >= m.rmse - 1e-12, "max {} >= rmse {}", m.max_abs_error, m.rmse);
		assert!(m.rmse >= m.mae - 1e-12, "rmse {} >= mae {}", m.rmse, m.mae);
		assert!(m.mae >= 0.0);
		assert!(m.bias.abs() <= m.mae + 1e-12, "|bias| {} <= mae {}", m.bias.abs(), m.mae);
	}

	#[test]
	fn length_mismatch_is_rejected() {
		let predicted = points_with_values(&[1.0, 2.0]);
		let err = AccuracyMetrics::from_aligned(&predicted, &[1.0]).expect_err("mismatch must error");
		assert_eq!(err, AccuracyError::LengthMismatch { predicted: 2, truth: 1 });
	}

	#[test]
	fn empty_input_is_rejected() {
		let err = AccuracyMetrics::from_aligned(&[], &[]).expect_err("empty must error");
		assert_eq!(err, AccuracyError::Empty);
	}

	#[test]
	fn a_non_finite_prediction_is_rejected() {
		// `1e400` overflows the `f64` conversion, so it is not a finite prediction.
		let predicted = vec![Point { timestamp: Utc.timestamp_opt(0, 0).single().unwrap(), value: "1e400".parse::<BigDecimal>().unwrap() }];
		let err = AccuracyMetrics::from_aligned(&predicted, &[0.0]).expect_err("non-finite must error");
		assert_eq!(err, AccuracyError::NonFinite { index: 0 });
	}

	#[test]
	fn synthetic_ground_truth_is_finite_in_range_and_aligns_with_the_grid() {
		let profile = InterpolationProfile::interpolation_heavy_irregular();
		let truth = synthetic_ground_truth(&profile).expect("generated profile has ground truth");
		let expected = generate_target_times(profile.start(), profile.end(), profile.resolution).len();
		assert_eq!(truth.len(), expected, "ground truth must cover the whole reconstruction grid");
		assert!(!truth.is_empty());
		for v in truth {
			assert!(v.is_finite() && (10.0..=90.0).contains(&v), "every truth value must be finite in [10, 90], got {v}");
		}
	}

	#[test]
	fn synthetic_ground_truth_refuses_a_line_protocol_profile() {
		let payload = "cpu,host=h0 usage=10.0 0\ncpu,host=h0 usage=12.0 120\n";
		let profile = InterpolationProfile::from_line_protocol("tsbs-cpu", payload, "usage", TimestampPrecision::Seconds, Spline::Cubic, Resolution::Minutes).expect("valid payload");
		let err = synthetic_ground_truth(&profile).expect_err("line-protocol has no ground truth");
		assert_eq!(err, AccuracyError::NoGroundTruth);
	}
}
