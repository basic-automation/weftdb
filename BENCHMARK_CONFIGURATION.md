# Criterion Benchmark Configuration Guide

This guide explains how to configure Criterion benchmarks to control execution time and sample counts.

## Methods to Configure Benchmarks

### 1. Using Benchmark Groups (Recommended)

```rust
fn benchmark_function(c: &mut Criterion) {
    // Create a benchmark group with custom configuration
    let mut group = c.benchmark_group("my_benchmark_group");
    
    // Configure parameters
    group.sample_size(10);                                              // Number of samples (default: 100)
    group.measurement_time(std::time::Duration::from_secs(2));         // Time spent measuring (default: 5s)
    group.warm_up_time(std::time::Duration::from_secs(1));            // Warm-up time (default: 3s)
    
    // Optional: Configure significance level and confidence level
    group.significance_level(0.05);                                    // Statistical significance level
    group.confidence_level(0.95);                                      // Confidence level for measurements
    
    // Optional: Set noise threshold
    group.noise_threshold(0.02);                                       // 2% noise threshold
    
    // Run benchmarks
    group.bench_function("my_function", |b| {
        b.iter(|| {
            // Your benchmark code here
        });
    });
    
    // Always finish the group
    group.finish();
}
```

### 2. Global Criterion Configuration

```rust
use criterion::{Criterion, SamplingMode, BenchmarkId};

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)                                               // Global sample size
        .measurement_time(std::time::Duration::from_secs(3))          // Global measurement time
        .warm_up_time(std::time::Duration::from_secs(1))              // Global warm-up time
        .sampling_mode(SamplingMode::Flat);                           // Sampling mode
    targets = my_benchmark_function
}
```

### 3. Environment Variables

You can also control Criterion behavior using environment variables:

```bash
# Set sampling mode
export CRITERION_SAMPLING_MODE=flat

# Set sample size
export CRITERION_SAMPLE_SIZE=20

# Disable HTML reports for faster execution
export CRITERION_NO_REPORTS=1
```

### 4. Command Line Options

```bash
# Run specific benchmark
cargo bench --bench cache_performance_benchmarks

# Run with reduced verbosity
cargo bench -- --quiet

# Run specific benchmark function
cargo bench cache_invalidation

# Generate baseline
cargo bench -- --save-baseline my_baseline

# Compare with baseline
cargo bench -- --baseline my_baseline
```

## Configuration Parameters Explained

### Sample Size
- **Default**: 100 samples
- **Purpose**: Number of iterations to measure
- **Impact**: More samples = more accurate but slower
- **Recommended**: 10-50 for slow benchmarks, 100+ for fast ones

### Measurement Time
- **Default**: 5 seconds
- **Purpose**: Total time spent collecting measurements
- **Impact**: Longer time = more accurate but slower
- **Recommended**: 2-3 seconds for development, 5+ for CI

### Warm-up Time  
- **Default**: 3 seconds
- **Purpose**: Time to let the system stabilize before measuring
- **Impact**: Longer warm-up = more consistent results
- **Recommended**: 1 second for most benchmarks

### Significance Level
- **Default**: 0.05 (5%)
- **Purpose**: Statistical significance threshold for detecting changes
- **Impact**: Lower = more sensitive to small changes

### Confidence Level
- **Default**: 0.95 (95%)
- **Purpose**: Statistical confidence in measurements
- **Impact**: Higher = more reliable but potentially slower

## Example Configurations by Benchmark Type

### Fast Benchmarks (< 1ms per iteration)
```rust
group.sample_size(100);
group.measurement_time(std::time::Duration::from_secs(5));
group.warm_up_time(std::time::Duration::from_secs(3));
```

### Medium Benchmarks (1-100ms per iteration)
```rust
group.sample_size(50);
group.measurement_time(std::time::Duration::from_secs(3));
group.warm_up_time(std::time::Duration::from_secs(2));
```

### Slow Benchmarks (100ms+ per iteration)
```rust
group.sample_size(10);
group.measurement_time(std::time::Duration::from_secs(2));
group.warm_up_time(std::time::Duration::from_secs(1));
```

### Very Slow Benchmarks (1s+ per iteration)
```rust
group.sample_size(10); // Minimum 10 samples required by Criterion
group.measurement_time(std::time::Duration::from_secs(1));
group.warm_up_time(std::time::Duration::from_secs(1));
```

## Interpreting Criterion Warnings

### "Unable to complete X samples in Ys"
**Solution**: Reduce sample_size or increase measurement_time
```rust
// Before
group.sample_size(100);
group.measurement_time(std::time::Duration::from_secs(5));

// After  
group.sample_size(20);
group.measurement_time(std::time::Duration::from_secs(10));
```

### "You may wish to increase target time to Xs"
**Solution**: Increase measurement_time to suggested value or reduce sample_size
```rust
// If Criterion suggests 15s target time:
group.measurement_time(std::time::Duration::from_secs(15));
// OR reduce samples:
group.sample_size(10);
```

### "or reduce sample count to X"
**Solution**: Use the suggested sample count
```rust
// If Criterion suggests 10 samples:
group.sample_size(10);
```

## Best Practices

1. **Start with conservative settings** for new benchmarks
2. **Use groups** rather than global configuration for flexibility
3. **Profile your benchmarks** - run with `--debug` to see timing
4. **Use baselines** for regression testing
5. **Document your configuration choices** in comments
6. **Test locally** before committing benchmark changes
7. **Consider CI time limits** when setting parameters

## Applied Example: Cache Performance Benchmarks

Our cache performance benchmarks use these optimized settings:

```rust
// Fast cache operations (ns-scale)
let mut cache_group = c.benchmark_group("cache_performance");
// Uses default settings - these are fast enough

// Slow cache operations (ms-scale)  
let mut group = c.benchmark_group("cache_invalidation_group");
group.sample_size(20);
group.measurement_time(std::time::Duration::from_secs(2));
group.warm_up_time(std::time::Duration::from_secs(1));

// Very slow operations (second-scale)
let mut group = c.benchmark_group("cache_eviction_group");
group.sample_size(10);
group.measurement_time(std::time::Duration::from_secs(2));
group.warm_up_time(std::time::Duration::from_secs(1));
```

This reduces benchmark execution time from ~10 minutes to ~2-3 minutes while maintaining statistical validity.