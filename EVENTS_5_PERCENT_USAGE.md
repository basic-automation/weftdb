# 5% Monthly Price Increase Event Detection

This document describes how to use the `build_events_5_percent_queue` function to detect 5% price increases from the start to the end of calendar months.

## Overview

The `build_events_5_percent_queue` function analyzes price movements to detect when the price rises 5% or more from the beginning of a calendar month to the end of that same month. Each detected increase creates an event with a manifestation at the end of the month when the 5% threshold is confirmed.

This is particularly useful for determining if a "buy at the start of each month" investment strategy would be profitable.ice Increase Event Detection

This document describes how to use the `build_events_5_percent_queue` function to detect 5% price increases within a month timeframe and create events.

## Overview

The `build_events_5_percent_queue` function analyzes price measurements using a sliding window approach to detect when the price rises 5% or more within a month period. Each detected increase creates an event with a manifestation at the time when the 5% threshold is reached.

## Function Signature

```rust
pub async fn build_events_5_percent_queue(
    database: &Database, 
    aspect: &AspectId, 
    resolution: &Resolution, 
    method: &Spline
) -> Result<()>
```

## Parameters

- **database**: Database instance to query measurements from
- **aspect**: The aspect ID to analyze for price movements  
- **resolution**: The resolution for data analysis (e.g., `Resolution::Hours`, `Resolution::Days`)
- **method**: The spline interpolation method to use (e.g., `Spline::Linear`, `Spline::Cubic`)

## Algorithm

1. **Data Retrieval**: Retrieves all measurements for the aspect within the date range
2. **Monthly Grouping**: Groups measurements by calendar month (year-month pairs)
3. **Start/End Price Analysis**: For each month, finds the first price (beginning) and last price (end)
4. **Percentage Calculation**: Calculates percentage increase from start to end of month: `(end_price - start_price) / start_price`
5. **Threshold Detection**: If increase is 5% or higher, creates an event at the end of the month
6. **Event Storage**: Adds events to the global `EVENTS_QUEUE`

## Usage Example

```rust
use dataset_management::{build_events_5_percent_queue, get_events_queue, clear_events_queue, analyze_event_timing};
use database::{Database, AspectId};
use splimes::{Resolution, Spline};

async fn detect_and_analyze_price_events() -> anyhow::Result<()> {
    // Create database and get aspect
    let db = Database::existing("my_financial_data").await?;
    let aspect_id = AspectId::from_uuid(uuid); // Your price aspect ID
    
    // Clear previous events if needed
    clear_events_queue().await;
    
    // Detect 5% monthly price increases
    build_events_5_percent_queue(
        &db, 
        &aspect_id, 
        &Resolution::Hours, 
        &Spline::Linear
    ).await?;
    
    // Retrieve detected events
    let events = get_events_queue().await;
    
    println!("Detected {} monthly price increase events", events.len());
    
    for event in &events {
        println!("Event: {} at {}", 
            event.name, 
            event.manifestations[0].start.format("%Y-%m-%d")
        );
        
        if let Some(description) = &event.description {
            println!("  Description: {}", description);
        }
        
        // Access timing information
        let manifestation = &event.manifestations[0];
        println!("  Duration: {:.1} days", manifestation.duration_days());
        println!("  Midpoint: {}", manifestation.midpoint().format("%Y-%m-%d %H:%M"));
        
        // Check if event contains specific dates (useful for correlation)
        let month_start = manifestation.start.with_day(1).unwrap();
        let month_end = manifestation.end.with_day(28).unwrap(); // Safe day for all months
        
        println!("  Contains month start: {}", manifestation.contains(month_start));
        println!("  Contains month end: {}", manifestation.contains(month_end));
    }
    
    // Run comprehensive timing analysis
    analyze_event_timing().await?;
    
    Ok(())
}
```

## Event Structure

Each detected event has the following structure:

```rust
Event {
    id: Uuid,                    // Unique event identifier
    name: String,                // "5% Monthly Price Increase - YYYY-MM"
    description: Some(String),   // Details about the monthly price increase
    manifestations: Vec<Manifestation>, // When and where it occurred (end of month)
    correlations: Vec<Correlation>,     // Empty initially - populated during pattern correlation
}
```

### Manifestation Structure

```rust
Manifestation {
    dataset_id: Uuid,           // Database identifier
    start: DateTime<Utc>,       // Start of the month when event begins
    end: DateTime<Utc>,         // End of the month when event completes
}
```

### Manifestation Utility Methods

The `Manifestation` struct provides several useful methods for working with event timing:

```rust
impl Manifestation {
    // Create a new manifestation
    fn new(dataset_id: Uuid, start: DateTime<Utc>, end: DateTime<Utc>) -> Self;
    
    // Get the duration of the event
    fn duration(&self) -> chrono::Duration;
    
    // Get the midpoint timestamp
    fn midpoint(&self) -> DateTime<Utc>;
    
    // Check if a timestamp falls within the event
    fn contains(&self, timestamp: DateTime<Utc>) -> bool;
    
    // Get duration in days as floating point
    fn duration_days(&self) -> f64;
    
    // Get duration in hours as floating point  
    fn duration_hours(&self) -> f64;
}
```

## Helper Functions

### `get_events_queue() -> Vec<Event>`

Retrieves all events from the events queue. Returns a clone of all events to avoid blocking the queue.

### `clear_events_queue()`

Clears all events from the events queue. Useful for testing or when starting fresh event detection.

## Use Cases

### Financial Markets
```rust
// Detect significant price movements in stock/crypto data
build_events_5_percent_queue(&db, &btc_price_aspect, &Resolution::Hours, &Spline::Linear).await?;
```

### Manufacturing
```rust
// Detect cost increases in material prices
build_events_5_percent_queue(&db, &material_cost_aspect, &Resolution::Days, &Spline::Cubic).await?;
```

### Real Estate
```rust
// Detect property value increases
build_events_5_percent_queue(&db, &property_value_aspect, &Resolution::Weeks, &Spline::Linear).await?;
```

## Correlation Possibilities

The start/end timestamp structure enables sophisticated correlation analysis:

### Pattern Correlation Timing

With both start and end timestamps, you can correlate patterns with different phases of the event:

```rust
// Correlate patterns that occur before the event starts
let patterns_before = find_patterns_before(manifestation.start);

// Correlate patterns that occur at the beginning of the event  
let patterns_at_start = find_patterns_around(manifestation.start, tolerance);

// Correlate patterns that occur during the event
let patterns_during = find_patterns_between(manifestation.start, manifestation.end);

// Correlate patterns that occur at the end of the event
let patterns_at_end = find_patterns_around(manifestation.end, tolerance);

// Correlate patterns that occur after the event ends
let patterns_after = find_patterns_after(manifestation.end);
```

### Duration-Based Analysis

```rust
// Analyze events by duration
let short_events = events.iter().filter(|e| e.manifestations[0].duration_days() < 25.0);
let long_events = events.iter().filter(|e| e.manifestations[0].duration_days() > 35.0);

// Correlate with patterns based on event duration
if manifestation.duration_days() > 30.0 {
    // Look for different patterns in longer events
}
```

### Midpoint Analysis

```rust
// Find patterns that occur at the midpoint of profitable months
let midpoint = manifestation.midpoint();
let midpoint_patterns = find_patterns_around(midpoint, Duration::days(1));
```

## Performance Considerations

- The function streams data to handle large datasets efficiently
- Uses a sliding window approach with O(n²) complexity in worst case
- Consider using appropriate resolution settings to balance accuracy vs performance
- For very large datasets, consider processing data in chunks or using time-based filtering

## Error Handling

The function returns `Result<()>` and can fail if:
- Database access fails
- No measurements found for the aspect
- Streaming data encounters errors

Always handle these errors appropriately in your application.

## Testing

A comprehensive test is included that verifies:
- Event detection for 5% price increases
- Proper event structure creation
- Queue management functionality
- Database integration

Run tests with:
```bash
cargo test test_build_events_5_percent_queue -- --nocapture
```