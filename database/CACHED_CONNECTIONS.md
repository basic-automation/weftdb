# Cached Database Connections Implementation

## Overview

I've implemented a comprehensive caching system for database connections throughout the DSP database layer. This improves performance by reusing connections and provides MVCC concurrent transaction support.

## Key Changes

### 1. Enhanced DatabaseCache with Connection Support

- Added `connection_cache` to store database connections with TTL
- Added `get_connection()` and `store_connection()` methods
- Added LRU eviction for connection management
- Connections expire after 30 minutes to prevent staleness

### 2. Modified begin_concurrent Function

**New Signature:**
```rust
pub async fn begin_concurrent(turso_db: &turso::Database, cache_key: &str) -> Result<CachedConnection>
```

**Key Features:**
- Takes a database and cache key, returns a cached connection
- Automatically starts MVCC concurrent transactions
- Caches connections for reuse across operations
- Falls back to BEGIN IMMEDIATE or BEGIN for compatibility

**Legacy Support:**
```rust
pub async fn begin_concurrent_direct(conn: &turso::Connection) -> bool
```
- Maintains backward compatibility for existing code

### 3. Updated Core Database Operations

**Examples Implemented:**

#### Input Operations (inputs.rs)
```rust
// Before
let conn = measurement_db.connect()?;
let began = Self::begin_concurrent_direct(&conn).await;

// After  
let cache_key = format!("measurement_db_{}_{}", aspect_id, dataset_id);
let conn = Self::begin_concurrent(&measurement_db, &cache_key).await?;
```

#### Query Operations (outputs.rs)
```rust
// Before
let conn = measurement_db.connect()?;

// After
let cache_key = format!("measurement_db_query_{}_{}_{}_{}", aspect_id, start_time.timestamp_millis(), end_time.timestamp_millis(), max_per_page);
let conn = Self::begin_concurrent(&measurement_db, &cache_key).await?;
```

## Benefits

### 1. **Performance Improvements**
- **Connection Reuse**: Eliminates overhead of creating new connections
- **Reduced Latency**: Cached connections respond faster
- **Memory Efficiency**: LRU eviction prevents unlimited memory growth

### 2. **Concurrency Support** 
- **MVCC Transactions**: Automatic BEGIN CONCURRENT for optimal concurrency
- **Lock Reduction**: Better handling of concurrent database access
- **Retry Logic**: Built-in retry mechanisms for transient failures

### 3. **Resource Management**
- **TTL Expiration**: Connections expire after 30 minutes
- **Cache Limits**: Maximum 50 cached connections by default
- **Automatic Cleanup**: Periodic cleanup of expired connections

## Cache Key Patterns

### Input Operations
- `measurement_db_{aspect_id}_{dataset_id}` - For measurement captures
- `measurement_db_new_{aspect_id}_{dataset_id}` - For new-only measurements

### Query Operations  
- `measurement_db_query_{aspect_id}_{start_time}_{end_time}_{max_per_page}` - For queries
- `batch_db_{aspect_id}` - For batch operations
- `pattern_db_{aspect_id}` - For pattern operations

### Metadata Operations
- `metadata_db_{database_id}` - For database metadata
- `subject_db_{database_id}_{subject_id}` - For subject operations

## Migration Strategy

### Phase 1: Foundation (✅ Complete)
- Implemented connection caching infrastructure
- Created new `begin_concurrent` with cache support
- Added legacy `begin_concurrent_direct` for compatibility
- Updated core input/output operations

### Phase 2: Systematic Conversion
- Convert measurement operations to use cached connections
- Convert batch operations to use cached connections  
- Convert pattern operations to use cached connections
- Convert correlation operations to use cached connections

### Phase 3: Optimization
- Fine-tune cache sizes and TTL values
- Add monitoring and metrics
- Performance testing and benchmarking

## Usage Examples

### Write Operations
```rust
async fn capture_measurement(&self, aspect_id: AspectId, dataset_id: DatasetId, input_measurement: InputMeasurement) -> Result<TxId> {
    let mut aspect = self.get_aspect(aspect_id).await?;
    let measurement_db = aspect.measurements().await?;
    let cache_key = format!("measurement_db_{}_{}", aspect_id, dataset_id);
    
    let conn = Self::begin_concurrent(&measurement_db, &cache_key).await?;
    
    // Execute operations using conn.as_ref()
    let res = conn.as_ref().execute(sql, params).await;
    
    // Commit/rollback using cached connection methods
    Self::commit_concurrent_direct(conn.as_ref()).await?;
    
    Ok(tx_id)
}
```

### Read Operations
```rust
async fn get_measurements(&self, aspect_id: AspectId) -> Result<Vec<Measurement>> {
    let mut aspect = self.get_aspect(aspect_id).await?;
    let measurement_db = aspect.measurements().await?;
    let cache_key = format!("measurement_db_query_{}", aspect_id);
    
    let conn = Self::begin_concurrent(&measurement_db, &cache_key).await?;
    let mut rows = conn.as_ref().query(sql, params).await?;
    
    // Process results
    Ok(measurements)
}
```

## Configuration

### Cache Settings (in cache.rs)
```rust
max_connection_entries: 50,           // Maximum cached connections
connection_ttl: Duration::from_secs(1800), // 30 minute TTL
```

### Retry Settings
```rust
max_attempts: 5,                      // Maximum retry attempts
backoff: 10-15ms * attempt           // Exponential backoff
```

## Next Steps

1. **Complete Migration**: Convert remaining database operations to use cached connections
2. **Performance Testing**: Benchmark before/after performance improvements  
3. **Monitoring**: Add connection pool metrics and health checks
4. **Documentation**: Update API documentation with new patterns
5. **Testing**: Add integration tests for cached connection scenarios

## Files Modified

- `database/src/types/cache.rs` - Added connection caching infrastructure
- `database/src/types/database/mod.rs` - Modified begin_concurrent functions
- `database/src/types/database/inputs.rs` - Updated input operations
- `database/src/types/database/outputs.rs` - Updated query operations
- All other database modules - Updated to use begin_concurrent_direct for compatibility

This implementation provides a solid foundation for high-performance, concurrent database operations while maintaining backward compatibility.