# WARP.md

This file provides guidance to WARP (warp.dev) when working with code in this repository.

## Project Overview

DSP is a high-performance time-series database system with advanced interpolation capabilities, optimized for real-time sensor data processing and analysis. The project consists of three main Rust crates organized as a Cargo workspace:

- **`database`**: Core time-series database with SQLite backend, providing ACID guarantees and high-performance data ingestion/querying
- **`splimes`**: Advanced spline interpolation library with GPU acceleration support using WGPU
- **`dataset_management`**: Higher-level APIs for batch processing and dataset operations

## Common Development Commands

### Building
```bash
# Build all workspace members
cargo build

# Build in release mode (optimized)
cargo build --release

# Build specific crate
cargo build -p database
cargo build -p splimes
cargo build -p dataset_management
```

### Testing
```bash
# Run all tests
cargo test

# Run tests for specific crate
cargo test -p database
cargo test -p splimes
cargo test -p dataset_management

# Run tests with output
cargo test -- --nocapture
```

### Benchmarks
```bash
# Run database benchmarks
cargo bench -p database

# Run splimes interpolation benchmarks
cargo bench -p splimes

# Specific benchmark suites
cargo bench -p database interpolation_benchmarks
cargo bench -p database strategy_selection_benchmarks
cargo bench -p database cache_performance_benchmarks
```

### Code Formatting
```bash
# Format all code (uses custom rustfmt.toml settings)
cargo fmt

# Check formatting without applying changes
cargo fmt -- --check
```

### Linting
```bash
# Run clippy for all targets
cargo clippy

# Run clippy with all features
cargo clippy --all-features

# Run clippy for specific crate
cargo clippy -p splimes
```

### Documentation
```bash
# Generate documentation
cargo doc

# Generate and open documentation
cargo doc --open

# Include private items in documentation
cargo doc --document-private-items
```

## Architecture Overview

### Data Flow
1. **Input**: Time-series data enters through the `database` crate's measurement APIs
2. **Storage**: Data is persisted in SQLite with optimized schemas for time-series queries
3. **Processing**: The `splimes` crate provides interpolation with automatic CPU/GPU strategy selection
4. **Batch Operations**: The `dataset_management` crate handles large-scale batch processing

### Key Components

#### Database Crate (`database/`)
- **Core Types**: `Database`, `Subject`, `Aspect`, `InputMeasurement`, `Resolution`, `Spline`
- **Storage**: SQLite-based with connection pooling (default 5 connections)
- **Performance**: Intelligent caching system, ACID guarantees, optimized for real-time operations
- **API**: Async-first design with comprehensive error handling

#### Splimes Crate (`splimes/`)
- **Interpolation Engine**: Supports Linear, Cubic, and other spline methods
- **GPU Acceleration**: WGPU-based GPU computing with automatic fallback to CPU
- **Strategy Selection**: Intelligent algorithm selection based on dataset size and hardware
- **Performance Thresholds**: 
  - GPU Primary: Very large datasets (>1M points)
  - GPU with Timeout: Large datasets (competitive performance)
  - Parallel CPU: Medium datasets
  - Single-threaded CPU: Small datasets

#### Dataset Management Crate (`dataset_management/`)
- **Batch Processing**: Handles large datasets by creating manageable batches
- **Integration**: Bridges between database operations and splimes interpolation
- **Validation**: Ensures resolution compatibility and data integrity

### Performance Characteristics
- **Insert Performance**: 0.1-0.5ms (single), 10-50ms (batch of 1000)
- **Query Performance**: 0.1-1ms (point lookup), 1-100ms (range scan)
- **Interpolation**: 5-50ms (1K points), 100-500ms (GPU 1M points)
- **Memory**: Adaptive caching based on available system resources

## Code Style and Conventions

### Formatting Rules (rustfmt.toml)
- **Max Width**: 10000 characters (very wide lines allowed)
- **Tabs**: Hard tabs with 8-space width
- **Imports**: Horizontal layout, crate-level granularity, grouped StdExternalCrate
- **Ordering**: Reorder imports and impl items

### Clippy Configuration
The `splimes` crate uses strict clippy linting:
```rust
#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
```
With specific exceptions for multi-crate versions and naming patterns.

## Development Workflow

### Running Examples
Since this is a library-focused project, use the comprehensive examples in the database crate documentation:
```bash
# Run doctests to verify examples
cargo test --doc -p database
```

### Debugging
- VS Code launch configurations are provided for debugging
- Use `LLDB` for native debugging support
- Test configurations available for library debugging

### GPU Development
- GPU features require compatible NVIDIA hardware with CUDA support
- Fallback mechanisms ensure functionality without GPU
- Use `should_use_gpu()` helper to understand strategy selection

### Benchmarking Strategy
- Run benchmarks on representative hardware before performance optimization
- Pay attention to strategy selection benchmarks for GPU vs CPU decisions
- Use release builds for accurate performance measurements

## Dependencies and Features

### Core Dependencies
- **async**: tokio (full features), futures
- **database**: sqlx (SQLite), chrono, uuid, bigdecimal
- **gpu**: wgpu, bytemuck, pollster
- **performance**: rayon (parallelism), wide (SIMD), num_cpus
- **testing**: criterion (benchmarks), fake (test data generation), tempfile

### Optional Features
- GPU acceleration (graceful fallback if unavailable)
- SIMD optimizations (automatic detection)
- Parallel processing (scales with CPU cores)

## Troubleshooting

### Common Issues
1. **GPU Initialization Failures**: Check WGPU compatibility, fallback to CPU automatic
2. **SQLite Lock Issues**: Verify connection pool settings, check concurrent access patterns
3. **Memory Usage**: Monitor adaptive cache, adjust batch sizes if needed
4. **Performance**: Run strategy selection benchmarks to verify optimal paths

### ICE Files
The repository contains several `rustc-ice-*.txt` files indicating internal compiler errors during development. These suggest the codebase pushes Rust compiler limits, particularly around:
- Complex generic types
- Heavy use of async/await
- GPU shader compilation

When encountering similar issues, try:
- Simplifying generic constraints
- Breaking complex functions into smaller parts
- Using stable Rust toolchain versions
