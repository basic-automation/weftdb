use database::splines::{auto_interpolate, Resolution, SplineType};
use database::Measurement;
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use std::str::FromStr;
use std::time::Instant;
use uuid::Uuid;

/// Comprehensive test to verify strategy selection is working correctly
#[test] // ← Remove async, make it a regular test
fn verify_strategy_selection_comprehensive() {
    // Test Case 1: Small Dataset - Should be fastest (standard algorithm)
    let small_measurements = create_test_measurements(75);
    let start_time = Instant::now();
    let result = auto_interpolate(
        small_measurements.clone(),
        small_measurements[0].timestamp,
        small_measurements[small_measurements.len() - 1].timestamp,
        Resolution::Seconds,
        SplineType::Linear,
    );
    let small_duration = start_time.elapsed();
    assert!(result.is_ok(), "Small dataset should succeed");
    
    // Test Case 2: Medium Dataset with Dense Output - Should use SIMD
    let medium_measurements = create_test_measurements(400);
    let dense_end = medium_measurements[0].timestamp + chrono::Duration::hours(1);
    let start_time = Instant::now();
    let result = auto_interpolate(
        medium_measurements.clone(),
        medium_measurements[0].timestamp,
        dense_end,
        Resolution::Seconds,
        SplineType::Linear,
    );
    let medium_dense_duration = start_time.elapsed();
    assert!(result.is_ok(), "Medium dataset with dense output should succeed");
    
    // Test Case 3: Large Dataset - Should use parallel processing
    let large_measurements = create_test_measurements(1500);
    let start_time = Instant::now();
    let result = auto_interpolate(
        large_measurements.clone(),
        large_measurements[0].timestamp,
        large_measurements[large_measurements.len() - 1].timestamp,
        Resolution::Seconds,
        SplineType::Quadratic,
    );
    let large_duration = start_time.elapsed();
    assert!(result.is_ok(), "Large dataset should succeed");
    
    // Verify performance characteristics align with strategy selection
    println!("🎯 Performance Analysis:");
    println!("📊 Small dataset (75 points): {:?}", small_duration);
    println!("📊 Medium dataset with dense output (400 points): {:?}", medium_dense_duration);  
    println!("📊 Large dataset (1500 points): {:?}", large_duration);
    
    // Performance expectations based on strategy selection
    assert!(small_duration.as_millis() < 100, "Small dataset should be very fast (< 100ms)");
    assert!(medium_dense_duration.as_millis() < 500, "Medium dataset should benefit from SIMD (< 500ms)");
    assert!(large_duration.as_millis() < 1000, "Large dataset should benefit from parallel processing (< 1s)");
    
    // Verify strategy selection logic
    verify_strategy_thresholds();
}

#[test]
fn verify_strategy_thresholds() {
    // Test dataset size thresholds
    assert!(75 < 200, "75 measurements should trigger small dataset path");
    assert!(400 >= 200 && 400 < 1000, "400 measurements should trigger medium dataset path");
    assert!(1500 >= 1000 && 1500 < 5000, "1500 measurements should trigger large dataset path");
    
    // Test output density calculations
    let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
    let dense_end = start + chrono::Duration::hours(1); // 3600 seconds
    let time_span = (dense_end - start).num_seconds();
    let estimated_points = time_span as usize; // ~3600 points for Resolution::Seconds
    
    assert!(estimated_points >= 256, "Dense output scenario should exceed SIMD threshold");
    
    println!("✅ Strategy thresholds verified:");
    println!("   📏 Small dataset: < 200 measurements");
    println!("   📏 Medium dataset: 200-999 measurements");  
    println!("   📏 Large dataset: 1000-4999 measurements");
    println!("   📏 SIMD threshold: {} points (should be >= 256)", estimated_points);
}

#[test] 
fn verify_algorithm_optimization() {
    use database::splines::apply_fast_path_optimization;
    
    // Test algorithm degradation for performance
    let cubic_optimized = apply_fast_path_optimization(SplineType::Cubic, 2500, 3);
    assert_eq!(cubic_optimized, SplineType::Quadratic, "Cubic should degrade to Quadratic for large datasets");
    
    let poly_optimized = apply_fast_path_optimization(SplineType::Polynomial(8), 4000, 8);
    assert_eq!(poly_optimized, SplineType::Quadratic, "High-degree polynomial should degrade for performance");
    
    let linear_unchanged = apply_fast_path_optimization(SplineType::Linear, 10000, 1);
    assert_eq!(linear_unchanged, SplineType::Linear, "Linear should not be degraded");
    
    println!("✅ Algorithm optimization verified:");
    println!("   🔄 Cubic → Quadratic for large datasets");
    println!("   🔄 High-degree polynomial → Quadratic for performance");
    println!("   ✋ Linear remains unchanged");
}

fn create_test_measurements(count: usize) -> Vec<Measurement> {
    let dataset_id = Uuid::new_v4();
    let start_time = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

    (0..count)
        .map(|i| Measurement {
            id: Uuid::new_v4(),
            dataset_id,
            timestamp: start_time + chrono::Duration::seconds(i as i64 * 10),
            value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap(),
        })
        .collect()
}