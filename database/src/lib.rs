//! # High-Performance Time-Series Database
//!
//! A high-performance time-series database with advanced interpolation capabilities,
//! optimized for real-time sensor data processing and analysis.
//!
//! ## Quick Start
//!
//! ```rust
//! use database::*;
//! use bigdecimal::BigDecimal;
//! use std::str::FromStr;
//! use chrono::TimeZone;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! # // Clean up any existing test data first
//! # std::fs::remove_dir_all(format!("{}/my_experiment", DEFAULT_DATA_DIR)).ok();
//! #
//! // Create a new database
//! let db = Database::new("my_experiment").await?;
//!
//! // Add a subject
//! let subject = db.track_subject("participant_001").await?;
//!
//! // Track an aspect (e.g., heart rate) with resolution
//! let aspect = db.track_aspect(subject, "heart_rate", Resolution::Seconds).await?;
//!
//! // Capture multiple measurements for interpolation
//! let measurements = vec![
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(),
//!         BigDecimal::from_str("70.0").unwrap()
//!     ),
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 0).unwrap(),
//!         BigDecimal::from_str("72.5").unwrap()
//!     ),
//!     InputMeasurement::new(
//!         chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 2, 0).unwrap(),
//!         BigDecimal::from_str("75.0").unwrap()
//!     ),
//! ];
//!
//! for measurement in measurements {
//!     db.observe_measurement(aspect.clone(), measurement).await?;
//! }
//!
//! // Analyze data point (interpolate between existing measurements)
//! let time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 30).unwrap();
//! let data_point = db.analyze_point(aspect.id(), time, Resolution::Seconds, Spline::Linear).await?;
//!
//! println!("Interpolated value: {}", data_point.value);
//! println!("GPS Strategy recommendation: {:?}",
//!     splimes::helpers::should_use_gpu(1000, 1000));
//!
//! # // Clean up test data
//! # std::fs::remove_dir_all("data/my_experiment").ok();
//! # Ok(())
//! # }
//! ```
//!
//! ## Features
//!
//! - **High Performance**: Optimized for real-time data processing
//! - **Advanced Interpolation**: Support for linear, cubic, and other spline methods
//! - **GPU Acceleration**: Automatic strategy selection for optimal performance
//! - **Time Series Analysis**: Built-in support for trend analysis and batch processing
//! - **Flexible Resolution**: Support from nanoseconds to years
//!
//! ## Design Goals
//!
//! The primary design goal of this database is to provide high-performance ingestion
//! and querying of time-series data, with a focus on real-time analytics and monitoring.
//! This is achieved through a combination of efficient data structures, parallel processing,
//! and hardware acceleration (e.g., GPU support). Additionally, the database aims to be
//! user-friendly, with a simple and intuitive API, and flexible, supporting a wide range
//! of use cases and data types.
//!
//! ## Getting Involved
//!
//! Contributions are welcome! Please check out the [GitHub repository](https://github.com/yourusername/your-repo)
//! for more information on how to contribute, report issues, or request features.
//!
//! ## License
//!
//! This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
//!
//! ## Acknowledgments
//!
//! - Inspired by [TimescaleDB](https://www.timescale.com/), [InfluxDB](https://www.influxdata.com/),
//!   and other great time-series databases
//! - Built with [Rust](https://www.rust-lang.org/), [Tokio](https://tokio.rs/), and other awesome
//!   open-source projects
//!
//! ## Limitations
//!
//! - Currently, only supports a single-node setup (no built-in clustering or sharding)
//! - Limited support for complex queries (e.g., JOINs, subqueries) - focus on time-series
//!   analytics functions
//! - Some advanced features (e.g., continuous aggregates, data retention policies) are
//!   yet to be implemented
//!
//! ## Future Work
//!
//! - Improve query optimization and execution planning
//! - Add support for more advanced SQL features and time-series functions
//! - Implement data retention policies, continuous aggregates, and other advanced
//!   time-series database features
//! - Explore distributed architecture for horizontal scalability
//!
//! ## Performance Benchmarks
//!
//! | Operation                | Time (ms) | Notes                               |
//! |--------------------------|-----------|-------------------------------------|
//! | Insert (single)          | 0.1-0.5   | Depends on storage backend         |
//! | Insert (batch of 1000)   | 10-50     | Bulk insert optimization           |
//! | Query (point lookup)     | 0.1-1     | Indexed lookup                     |
//! | Query (range scan)       | 1-100     | Depends on range size              |
//! | Interpolation (1K points)| 5-50      | Depends on method and hardware     |
//! | GPU Interpolation (1M)   | 100-500   | Requires compatible GPU            |
//!
//! These benchmarks are representative and may vary depending on your hardware, data
//! characteristics, and workload patterns.
//!
//! ## Compatibility
//!
//! This database is designed to work with:
//!
//! - **Operating Systems**: Linux, macOS, Windows
//! - **Rust Version**: 1.70.0 or later
//! - **Hardware**: `x86_64`, ARM64 architectures
//! - **GPU Support**: NVIDIA GPUs with CUDA support (optional)
//!
//! ## FAQ
//!
//! **Q: Can I use this database for real-time applications?**
//!
//! A: Yes, this database is designed for real-time applications and can handle high-throughput
//! ingestion and low-latency querying. However, the exact performance will depend on your
//! specific use case, data volume, and hardware.
//!
//! **Q: How does this database compare to other time-series databases?**
//!
//! A: This database focuses on high-performance interpolation and advanced analytics
//! capabilities, particularly for sensor data and continuous measurements. While it may
//! not have all the enterprise features of larger databases like `TimescaleDB` or `InfluxDB`,
//! it offers unique capabilities for interpolation-heavy workloads and GPU acceleration.
//!
//! **Q: Can I migrate data from other databases?**
//!
//! A: While there's no built-in migration tool yet, you can export your data from other
//! databases in a compatible format (e.g., CSV, JSON) and then import it into this database
//! using the appropriate commands. Please refer to the documentation for detailed instructions
//! on data migration.
//!
//! **Q: What are the hardware requirements for running this database?**
//!
//! A: The hardware requirements depend on the size of your data, the complexity of your
//! queries, and the performance you expect. As a general guideline, for small to medium
//! datasets (up to a few million points), a modern laptop or desktop computer should be
//! sufficient. For larger datasets or more demanding workloads, a server-class machine
//! with a fast CPU, plenty of RAM, and SSD storage is recommended. If you plan to use the
//! GPU acceleration features, a compatible NVIDIA GPU with sufficient VRAM is also
//! recommended.
//!
//! **Q: How can I get help or support for using this database?**
//!
//! A: You can check the documentation, FAQs, and examples provided in the GitHub repository
//! for help with common issues and questions. If you need further assistance, you can
//! open an issue on the GitHub repository or contact the maintainers directly.
//!
//! **Q: Is this database production-ready?**
//!
//! A: This database is actively developed and continuously improved. While it's used in
//! various projects and applications, you should evaluate it thoroughly for your specific
//! use case and requirements before deploying it in a production environment. Please refer
//! to the documentation and test suites for more information on stability and reliability.
//!
//! **Q: Who maintains this database?**
//!
//! A: This database is maintained by [Your Name](https://github.com/yourusername) and
//! contributors. Please check the GitHub repository for the list of contributors and
//! maintainers.
//!
//! **Q: How can I contact the maintainers of this database?**
//!
//! A: You can contact the maintainers by opening an issue on the GitHub repository or
//! by contacting them directly through their GitHub profiles. Please note that response
//! times may vary depending on the nature of the inquiry and the availability of the
//! maintainers.
//!
//! **Q: What are some potential use cases for this database?**
//!
//! A: This database is suitable for a wide range of use cases involving time-series data,
//! such as `IoT` sensor data processing, financial market analysis, real-time monitoring
//! and alerting, historical data analysis, and more. Its high performance, advanced
//! interpolation and analytics capabilities, and flexible resolution make it ideal for
//! any application that requires efficient and accurate processing and analysis of
//! time-stamped data.
//!
//! **Q: What are the default settings and configurations for the database?**
//!
//! A: The default settings and configurations for the database are designed to provide a
//! balance between performance and usability for a wide range of use cases. Some of the
//! key default settings include:
//!
//! - **Data Directory**: `data/` (can be overridden with environment variable)
//! - **Connection Pool Size**: 5 connections per database
//! - **Cache Size**: Adaptive based on available memory
//! - **Interpolation Method**: Linear (fastest, good for most use cases)
//! - **Resolution**: Milliseconds (good balance between precision and performance)
//!
//! Most of these settings can be customized when creating databases, subjects, or aspects.
//!
//! **Q: How does the caching system work?**
//!
//! A: The database includes an intelligent caching system that automatically caches
//! frequently accessed data and interpolation results. The cache is designed to be
//! transparent to the user and automatically manages memory usage based on available
//! system resources. You don't need to manually manage the cache, but you can monitor
//! its performance through the provided metrics and logging.
//!
//! **Q: What are the data consistency and durability guarantees?**
//!
//! A: The database provides strong consistency for individual operations and uses `SQLite`
//! as the underlying storage engine, which provides ACID guarantees. Data is automatically
//! persisted to disk and can survive system crashes. However, like most databases, it's
//! the responsibility of the application and the users to define and enforce the appropriate
//! integrity constraints and to handle any data quality issues.
//!
//! **Q: What are the default settings and configurations for the database?**
//!
//! A: The default settings and configurations for the database are designed to provide a
//! balance between performance and usability for a wide range of use cases. Some of the
//! key default settings include:
//!

#![recursion_limit = "2048"]
#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]
#![feature(stmt_expr_attributes)]

mod types;

// Export all public types from the types module
// Re-export types from splimes that are commonly used
pub use splimes::{Point, Resolution, Spline};
pub use types::*;
