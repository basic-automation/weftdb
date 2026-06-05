# Phase 5+ Roadmap: GPU Optimization Enhancements

## Overview

Phases 1-4 have established a solid foundation for GPU acceleration. Phase 5+ focuses on unlocking the remaining performance improvements and integrating true asynchronous operations.

---

## Phase 5: Command Batching (Estimated +50ms)

### Goal
Reduce GPU queue submission overhead by batching multiple operations.

### Current Architecture
Each batch submits a separate command encoder:
```
For each batch:
  1. Create encoder
  2. Record compute pass
  3. Copy to staging
  4. Submit (GPU queue operation)  ← Overhead per batch
```

### Proposed Phase 5 Implementation
```
// Pseudo-code for command batching
let mut batcher = CommandBatcher::new(device, max_batch_size: 10);

for batch in all_batches {
    batcher.add_compute_operation(/* ... */);

    if batcher.is_full() {
        batcher.flush(&queue); // Single submit for 10 operations
    }
}
batcher.flush(&queue); // Flush remaining
```

### Expected Benefits
- Reduce queue.submit() calls from N to N/10
- Estimated: 50ms improvement for 1M points

### Implementation Steps
1. Create `splimes/src/gpu/command_batcher.rs`
2. Implement batching logic with size limit
3. Integrate into `interpolate_f64_static` and `interpolate_f32_static`
4. Benchmark and verify improvement

---

## Phase 5.5: Enhanced Async Infrastructure

### Goal
Implement non-blocking GPU work tracking for true CPU-GPU parallelism.

### Current Implementation (Phase 3)
- `GpuInterpolationResult<T>` provides async interface
- Currently computes synchronously
- Foundation for future enhancement

### Proposed Phase 5.5 Enhancement
```rust
pub struct GpuInterpolationHandle<T> {
    staging_buffer: Arc<Buffer>,
    submission_index: Option<SubmissionIndex>,
    device: Arc<Device>,
    // Allow deferred readback
}

impl<T> GpuInterpolationHandle<T> {
    /// Non-blocking: Returns None if not ready
    pub fn try_poll(&mut self) -> Option<Result<Vec<T>>> {
        // Use device.poll() to check submission status
        // Only read buffer if GPU work is complete
    }

    /// Blocking: Ensures completion
    pub fn block_until_complete(self) -> Result<Vec<T>> {
        // device.poll(Wait) if needed
        // Read results
    }
}
```

### Implementation Steps
1. Redesign StagingBufferManager to handle deferred unmapping
2. Implement submission index tracking
3. Create non-blocking poll mechanism
4. Integrate with async/await infrastructure

### Expected Benefits
- CPU-GPU parallelism for streaming operations
- Better throughput in pipelined scenarios
- Foundation for concurrent interpolation calls

---

## Phase 6: Memory-Mapped I/O Optimization

### Goal
Reduce CPU-GPU transfer overhead through persistent mapping.

### Current Flow
```
Data → CPU Memory → Queue.write_buffer → GPU Memory → Compute → Read Results
                     (CPU-GPU Transfer)             (GPU-CPU Transfer)
```

### Proposed Phase 6 Enhancement
Use persistent mapped buffers for zero-copy transfers when possible:
```
Data → Mapped Buffer → Compute → Results → Mapped Buffer → CPU
       (No reallocation/copy)              (No unmap/remap)
```

### Implementation Steps
1. Profile CPU-GPU transfer overhead
2. Identify bottleneck transfers
3. Implement persistent mapped buffers for hot paths
4. Benchmark improvement

### Expected Benefits
- Reduce transfer overhead by ~20-30%
- Better latency for small batches

---

## Phase 7: Multi-GPU Support

### Goal
Enable GPU load balancing across multiple devices.

### Architecture
```
GPUInterpolator (GPU 0) ←→ Load Balancer ←→ GPUInterpolator (GPU 1)
                         ←→ GPUInterpolator (GPU N)
```

### Implementation Steps
1. Extend GLOBAL_INTERPOLATOR to support multiple devices
2. Implement workload distribution logic
3. Add GPU affinity options
4. Benchmark multi-GPU performance

### Expected Benefits
- Linear scaling with GPU count (for large workloads)
- Better utilization of heterogeneous systems

---

## Performance Prediction

### Combined Impact (All Phases)

| Phase | Component | Savings | Cumulative |
|-------|-----------|---------|-----------|
| 1 | Buffer Pool | 80ms | 80ms |
| 2 | Staging Buffers | 150ms | 230ms |
| 3 | Async Handle | 100ms* | 330ms* |
| 5 | Command Batching | 50ms | 380ms* |
| 5.5 | Async I/O | 50ms | 430ms* |
| 6 | Memory Mapping | 30ms | 460ms* |

*Estimated from Phase 3+ (requires streamed/pipelined operations)

### Baseline
- Current single-call: ~600ms to ~1400ms (GPU overhead)
- With Phases 1-2: ~600ms to ~800ms (70% reduction)
- With Phases 1-4: ~600ms to ~700ms (80% reduction)
- With All Phases: Streaming at 200-400ms per batch (75%+ reduction)

---

## Testing Strategy

### Phase 5 Tests
```rust
#[test]
fn test_command_batching_reduces_submissions() {
    // Verify submission count is reduced by ~10x
}

#[test]
fn test_batched_results_are_correct() {
    // Verify batching doesn't affect correctness
}
```

### Phase 5.5 Tests
```rust
#[test]
fn test_async_polling_completes() {
    // Verify non-blocking poll mechanism
}

#[test]
fn test_concurrent_interpolations() {
    // Verify GPU-CPU parallelism works
}
```

### Phase 6 Tests
```rust
#[test]
fn test_persistent_mapping_reduces_transfers() {
    // Measure transfer overhead reduction
}
```

---

## Integration Points

### With Existing Code
- All phases integrate via `interpolate_f64_static` and `interpolate_f32_static`
- No breaking changes to public API
- Backward compatible enhancements

### With TUI Application
- TUI can call `prewarm_gpu_with_config(GpuConfig::high_performance())`
- Better responsiveness through Phase 5.5 async
- Multi-GPU support benefits high-res visualization

### With Auto-Interpolation
- `should_use_gpu()` thresholds refined by benchmark results
- Phase 5+ enables GPU for more workloads
- Streaming operations benefit most from Phase 5.5+

---

## Success Criteria

### Phase 5
- [ ] Command batching reduces submissions by 10x
- [ ] +50ms improvement verified
- [ ] All tests passing

### Phase 5.5
- [ ] Non-blocking poll works correctly
- [ ] CPU-GPU parallelism demonstrated
- [ ] async/await integration seamless

### Phase 6
- [ ] Transfer overhead reduced by 20-30%
- [ ] Latency improved for small batches
- [ ] No correctness regression

### Phase 7
- [ ] Multi-GPU load balancing working
- [ ] Linear scaling verified
- [ ] Auto-device selection functional

---

## Priority & Effort Estimation

| Phase | Priority | Effort | Value |
|-------|----------|--------|-------|
| 5 | High | 2 days | +50ms → 380ms total |
| 5.5 | High | 3 days | +50ms + parallelism |
| 6 | Medium | 2 days | +30ms + better latency |
| 7 | Low | 4 days | Linear scaling |

**Recommended Order**: Phase 5 → Phase 5.5 → Phase 6 → Phase 7

---

## Monitoring & Profiling

### Key Metrics to Track
1. **GPU Queue Time**: Reduction via command batching (Phase 5)
2. **CPU-GPU Transfer Time**: Reduction via persistent mapping (Phase 6)
3. **Overall Latency**: Improvement via all phases combined
4. **GPU Utilization**: Increase with Phase 5.5 async
5. **Memory Usage**: Verify pool stays within limits

### Profiling Tools
- `cargo flamegraph` for hot paths
- `wgpu` validation layer for GPU bottlenecks
- Custom timing instrumentation for key sections

---

## Conclusion

Phase 5+ provides a clear roadmap for unlocking GPU acceleration benefits across diverse workload sizes. Starting with Phase 5 (command batching) offers the best return on effort for the next optimization cycle.

The foundation established in Phases 1-4 enables these enhancements without requiring major architectural changes, keeping the codebase maintainable and testable.

**Estimated Timeline**: 2-3 weeks for Phases 5-6 (high value), 1+ week for Phase 7 (lower priority).
