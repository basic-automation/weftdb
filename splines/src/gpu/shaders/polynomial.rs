/// GPU compute shader for polynomial interpolation with configurable bounds
pub const POLYNOMIAL_INTERPOLATION_SHADER: &str = r"
@group(0) @binding(0) var<storage, read> input_times: array<f32>;
@group(0) @binding(1) var<storage, read> input_values: array<f32>;
@group(0) @binding(2) var<storage, read> target_times: array<f32>;
@group(0) @binding(3) var<storage, read_write> output_values: array<f32>;
@group(0) @binding(4) var<uniform> config: PolynomialConfig;

struct PolynomialConfig {
    offset: u32,           // Workgroup offset
    max_degree: u32,       // Maximum polynomial degree
    bounds_factor_bits: u32, // Bounds factor as f32 bits (NaN = unbounded)
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let index = global_id.x + config.offset;
    let target_count = arrayLength(&target_times);
    
    if (index >= target_count) {
        return;
    }
    
    let target_time = target_times[index];
    let input_count = arrayLength(&input_times);
    
    if (input_count == 0u) {
        output_values[index] = 0.0;
        return;
    }
    
    if (input_count == 1u) {
        output_values[index] = input_values[0];
        return;
    }
    
    // Check if target is exactly at a data point
    for (var i = 0u; i < input_count; i++) {
        if (abs(input_times[i] - target_time) < 1e-7) {
            output_values[index] = input_values[i];
            return;
        }
    }
    
    // Convert bounds factor from bits to f32
    let bounds_factor = bitcast<f32>(config.bounds_factor_bits);
    let is_bounded = !isnan(bounds_factor);
    
    // The input_times array is already sorted, so we can use first and last elements
    let first_time = input_times[0];
    let last_time = input_times[input_count - 1u];
    
    // Check if we're extrapolating
    let is_extrapolating = target_time < first_time || target_time > last_time;
    
    if (is_extrapolating && is_bounded) {
        // Calculate data bounds for extrapolation
        var min_value = input_values[0];
        var max_value = input_values[0];
        
        for (var i = 1u; i < input_count; i++) {
            min_value = min(min_value, input_values[i]);
            max_value = max(max_value, input_values[i]);
        }
        
        let range = max_value - min_value;
        let lower_bound = min_value - range * bounds_factor;
        let upper_bound = max_value + range * bounds_factor;
        
        // Apply extrapolation bounds
        if (target_time < first_time) {
            output_values[index] = lower_bound;  // Left extrapolation
        } else {
            output_values[index] = upper_bound;  // Right extrapolation
        }
        return;
    }
    
    // For interpolation OR unbounded extrapolation, use polynomial interpolation
    let safe_degree = determine_safe_degree(config.max_degree, input_count);
    let result = polynomial_interpolate(target_time, safe_degree);
    output_values[index] = result;
}

fn determine_safe_degree(requested_degree: u32, available_points: u32) -> u32 {
    // Ensure we have enough points
    if (available_points <= 1u) {
        return 0u;
    }
    
    // Maximum degree is limited by available points
    let max_degree_by_points = available_points - 1u;
    
    // For numerical stability, limit to reasonable degree
    let stability_cap = 8u;
    
    // Use the minimum of requested degree, available points limit, and stability cap
    return min(min(requested_degree, max_degree_by_points), stability_cap);
}

fn polynomial_interpolate(target_time: f32, degree: u32) -> f32 {
    let input_count = arrayLength(&input_times);
    let num_points = min(degree + 1u, input_count);
    
    if (num_points == 0u) {
        return 0.0;
    }
    
    if (num_points == 1u) {
        return input_values[0];
    }
    
    // Select optimal window of points
    let window_start = select_window(target_time, num_points);
    
    // Use Lagrange interpolation
    return lagrange_interpolate(target_time, window_start, num_points);
}

fn select_window(target_time: f32, num_points: u32) -> u32 {
    let input_count = arrayLength(&input_times);
    
    if (num_points >= input_count) {
        return 0u;  // Use all points
    }
    
    // Find the position where target_time would be inserted
    var insert_pos = 0u;
    for (var i = 0u; i < input_count; i++) {
        if (input_times[i] <= target_time) {
            insert_pos = i + 1u;
        } else {
            break;
        }
    }
    
    // Center the window around the insertion point
    let half_window = num_points / 2u;
    var start_idx = insert_pos - min(insert_pos, half_window);
    
    // Ensure we don't go out of bounds
    if (start_idx + num_points > input_count) {
        start_idx = input_count - num_points;
    }
    
    return start_idx;
}

fn lagrange_interpolate(target_time: f32, start_idx: u32, num_points: u32) -> f32 {
    var result = 0.0;
    
    // Lagrange interpolation formula
    for (var j = 0u; j < num_points; j++) {
        let j_idx = start_idx + j;
        let tj = input_times[j_idx];
        let yj = input_values[j_idx];
        
        var basis = 1.0;
        
        // Calculate Lagrange basis polynomial L_j(x)
        for (var k = 0u; k < num_points; k++) {
            if (k != j) {
                let k_idx = start_idx + k;
                let tk = input_times[k_idx];
                let denominator = tj - tk;
                
                // Skip if denominator is too small to avoid numerical issues
                if (abs(denominator) < 1e-12) {
                    basis = 0.0;
                    break;
                }
                
                basis *= (target_time - tk) / denominator;
            }
        }
        
        result += yj * basis;
    }
    
    return result;
}
";
