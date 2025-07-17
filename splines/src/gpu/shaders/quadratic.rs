/// GPU compute shader for quadratic interpolation
pub const QUADRATIC_INTERPOLATION_SHADER: &str = r"
const TIME_EPSILON: f32 = 1.0e-6;
const DENOMINATOR_EPSILON: f32 = 1.0e-10;

@group(0) @binding(0) var<storage, read> input_times: array<f32>;
@group(0) @binding(1) var<storage, read> input_values: array<f32>;
@group(0) @binding(2) var<storage, read> target_times: array<f32>;
@group(0) @binding(3) var<storage, read_write> output_values: array<f32>;
@group(0) @binding(4) var<uniform> offset: u32;

fn is_uniform_spacing(input_count: u32) -> bool {
    if (input_count < 3u) {
        return false;
    }
    
    let interval = input_times[1] - input_times[0];
    let tolerance = TIME_EPSILON;
    
    for (var i = 2u; i < input_count; i++) {
        let current_interval = input_times[i] - input_times[i - 1u];
        if (abs(current_interval - interval) > tolerance) {
            return false;
        }
    }
    
    return true;
}

fn quadratic_interpolate_uniform(target_time: f32, data_start: f32, data_end: f32, uniform_interval: f32) -> f32 {
    let input_count = arrayLength(&input_times);
    
    if (target_time < data_start) {
        // Backward extrapolation - match CPU logic exactly
        let dt = target_time - data_start;
        let t = dt / uniform_interval;
        
        // Use same Lagrange formula as CPU: L0(t) = t*(t-1)/2, L1(t) = 1-t², L2(t) = t*(t+1)/2
        let t_squared = t * t;
        let l0 = t * (t - 1.0) * 0.5;
        let l1 = 1.0 - t_squared;
        let l2 = t * (t + 1.0) * 0.5;
        
        return input_values[0] * l0 + input_values[1] * l1 + input_values[2] * l2;
    }
    
    if (target_time > data_end) {
        // Forward extrapolation - match CPU logic exactly
        let dt = target_time - data_end;
        let t = dt / uniform_interval;
        
        // Use last three points - match CPU logic
        let n = input_count;
        let l0 = t * (t - 1.0) * 0.5;
        let l1 = 1.0 - t * t;
        let l2 = t * (t + 1.0) * 0.5;
        
        return input_values[n-3u] * l0 + input_values[n-2u] * l1 + input_values[n-1u] * l2;
    }
    
    // Interpolation - match CPU uniform logic
    let time_from_start = target_time - data_start;
    
    // Find segment index using same logic as CPU
    var segment_index = 0u;
    if (time_from_start >= 0.0 && uniform_interval > 0.0) {
        let idx = u32(time_from_start / uniform_interval);
        segment_index = min(idx, input_count - 2u);
    }
    
    // Use same three-point selection as CPU
    var i0: u32;
    var i1: u32;
    var i2: u32;
    
    if (segment_index == 0u) {
        i0 = 0u;
        i1 = 1u;
        i2 = 2u;
    } else if (segment_index >= input_count - 1u) {
        i0 = input_count - 3u;
        i1 = input_count - 2u;
        i2 = input_count - 1u;
    } else {
        i0 = segment_index - 1u;
        i1 = segment_index;
        i2 = segment_index + 1u;
    }
    
    // Calculate t parameter relative to center point (matches CPU)
    let dt = target_time - input_times[i1];
    let t = dt / uniform_interval;
    
    // Use same Lagrange formula as CPU
    let t_squared = t * t;
    let l0 = t * (t - 1.0) * 0.5;
    let l1 = 1.0 - t_squared;
    let l2 = t * (t + 1.0) * 0.5;
    
    return input_values[i0] * l0 + input_values[i1] * l1 + input_values[i2] * l2;
}

fn quadratic_interpolate_general(target_time: f32) -> f32 {
    let input_count = arrayLength(&input_times);
    
    // Binary search for appropriate segment
    var left = 0u;
    var right = input_count - 1u;
    
    while (left < right - 1u) {
        let mid = left + (right - left) / 2u;
        if (input_times[mid] <= target_time) {
            left = mid;
        } else {
            right = mid;
        }
    }
    
    // Find center index - match CPU spline logic
    var center_idx = left;
    if (target_time > input_times[left]) {
        center_idx = min(left + 1u, input_count - 2u);
    }
    center_idx = max(center_idx, 1u);
    center_idx = min(center_idx, input_count - 2u);
    
    // Handle boundary conditions - match CPU spline logic
    var i0: u32;
    var i1: u32;
    var i2: u32;
    
    if (center_idx == 0u) {
        i0 = 0u;
        i1 = 1u;
        i2 = 2u;
    } else if (center_idx >= input_count - 1u) {
        i0 = input_count - 3u;
        i1 = input_count - 2u;
        i2 = input_count - 1u;
    } else {
        i0 = center_idx - 1u;
        i1 = center_idx;
        i2 = center_idx + 1u;
    }
    
    // Standard Lagrange interpolation for non-uniform data
    let t0 = input_times[i0];
    let t1 = input_times[i1];
    let t2 = input_times[i2];
    let v0 = input_values[i0];
    let v1 = input_values[i1];
    let v2 = input_values[i2];
    
    let denom0 = (t0 - t1) * (t0 - t2);
    let denom1 = (t1 - t0) * (t1 - t2);
    let denom2 = (t2 - t0) * (t2 - t1);
    
    // Check for degenerate cases
    if (abs(denom0) < DENOMINATOR_EPSILON || abs(denom1) < DENOMINATOR_EPSILON || abs(denom2) < DENOMINATOR_EPSILON) {
        return v1; // Use center value as fallback
    }
    
    // Calculate Lagrange basis polynomials
    let l0 = ((target_time - t1) * (target_time - t2)) / denom0;
    let l1 = ((target_time - t0) * (target_time - t2)) / denom1;
    let l2 = ((target_time - t0) * (target_time - t1)) / denom2;
    
    // Calculate final value
    return v0 * l0 + v1 * l1 + v2 * l2;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let index = global_id.x + offset;
    let target_count = arrayLength(&target_times);
    
    if (index >= target_count) {
        return;
    }
    
    let target_time = target_times[index];
    let input_count = arrayLength(&input_times);
    
    if (input_count < 3u) {
        // Insufficient points for quadratic interpolation
        output_values[index] = 0.0;
        return;
    }
    
    // Check if data is uniform by examining spacing
    let is_uniform = is_uniform_spacing(input_count);
    
    if (is_uniform) {
        // Use uniform spacing optimization
        let data_start = input_times[0];
        let data_end = input_times[input_count - 1u];
        let uniform_interval = input_times[1] - input_times[0];
        
        output_values[index] = quadratic_interpolate_uniform(target_time, data_start, data_end, uniform_interval);
    } else {
        // Use general interpolation
        output_values[index] = quadratic_interpolate_general(target_time);
    }
}
";
