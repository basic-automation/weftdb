/// GPU compute shader for polynomial interpolation with configurable degree
pub const POLYNOMIAL_INTERPOLATION_SHADER: &str = r"
@group(0) @binding(0) var<storage, read> input_times: array<f32>;
@group(0) @binding(1) var<storage, read> input_values: array<f32>;
@group(0) @binding(2) var<storage, read> target_times: array<f32>;
@group(0) @binding(3) var<storage, read_write> output_values: array<f32>;
@group(0) @binding(4) var<uniform> config: PolynomialConfig;

struct PolynomialConfig {
    max_degree: u32,        // Maximum polynomial degree (e.g., 8 for up to 8th degree)
    use_local_fitting: u32, // 1 to use local windows, 0 to use all points
    window_size: u32,       // Size of local window when use_local_fitting = 1
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let index = global_id.x;
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
    
    // Determine the actual degree to use
    let max_points = min(config.max_degree + 1u, input_count);
    let degree = max_points - 1u;
    
    if (config.use_local_fitting == 1u && input_count > config.window_size) {
        // Use local polynomial fitting
        output_values[index] = local_polynomial_interpolate(target_time, degree);
    } else {
        // Use global polynomial fitting
        output_values[index] = global_polynomial_interpolate(target_time, degree);
    }
}

fn global_polynomial_interpolate(target_time: f32, degree: u32) -> f32 {
    let input_count = arrayLength(&input_times);
    let num_points = min(degree + 1u, input_count);
    
    // Use evenly spaced points or all points if few enough
    var result = 0.0;
    
    if (num_points == input_count) {
        // Use all points with Lagrange interpolation
        for (var i = 0u; i < num_points; i++) {
            let li = lagrange_basis(target_time, i, num_points);
            result += li * input_values[i];
        }
    } else {
        // Select evenly spaced points
        let step = f32(input_count - 1u) / f32(num_points - 1u);
        for (var i = 0u; i < num_points; i++) {
            let idx = u32(f32(i) * step + 0.5);
            let safe_idx = min(idx, input_count - 1u);
            let li = lagrange_basis_subset(target_time, i, num_points, step);
            result += li * input_values[safe_idx];
        }
    }
    
    return result;
}

fn local_polynomial_interpolate(target_time: f32, degree: u32) -> f32 {
    let input_count = arrayLength(&input_times);
    let num_points = min(degree + 1u, config.window_size);
    
    // Find the best center point for local fitting
    var center_idx = 0u;
    var min_distance = abs(target_time - input_times[0]);
    
    for (var i = 1u; i < input_count; i++) {
        let distance = abs(target_time - input_times[i]);
        if (distance < min_distance) {
            min_distance = distance;
            center_idx = i;
        }
    }
    
    // Determine the window bounds
    let half_window = num_points / 2u;
    var start_idx: u32;
    var end_idx: u32;
    
    if (center_idx < half_window) {
        start_idx = 0u;
        end_idx = min(num_points, input_count);
    } else if (center_idx + half_window >= input_count) {
        end_idx = input_count;
        start_idx = max(0u, input_count - num_points);
    } else {
        start_idx = center_idx - half_window;
        end_idx = start_idx + num_points;
    }
    
    let actual_points = end_idx - start_idx;
    
    // Perform Lagrange interpolation on the local window
    var result = 0.0;
    for (var i = 0u; i < actual_points; i++) {
        let idx = start_idx + i;
        let li = lagrange_basis_local(target_time, i, actual_points, start_idx);
        result += li * input_values[idx];
    }
    
    return result;
}

fn lagrange_basis(t: f32, j: u32, n: u32) -> f32 {
    var result = 1.0;
    let tj = input_times[j];
    
    for (var k = 0u; k < n; k++) {
        if (k != j) {
            let tk = input_times[k];
            let denominator = tj - tk;
            if (abs(denominator) < 1e-10) {
                // Handle degenerate case
                return 0.0;
            }
            result *= (t - tk) / denominator;
        }
    }
    
    return result;
}

fn lagrange_basis_subset(t: f32, j: u32, n: u32, step: f32) -> f32 {
    var result = 1.0;
    let j_idx = u32(f32(j) * step + 0.5);
    let safe_j_idx = min(j_idx, arrayLength(&input_times) - 1u);
    let tj = input_times[safe_j_idx];
    
    for (var k = 0u; k < n; k++) {
        if (k != j) {
            let k_idx = u32(f32(k) * step + 0.5);
            let safe_k_idx = min(k_idx, arrayLength(&input_times) - 1u);
            let tk = input_times[safe_k_idx];
            let denominator = tj - tk;
            if (abs(denominator) < 1e-10) {
                return 0.0;
            }
            result *= (t - tk) / denominator;
        }
    }
    
    return result;
}

fn lagrange_basis_local(t: f32, j: u32, n: u32, start_idx: u32) -> f32 {
    var result = 1.0;
    let tj = input_times[start_idx + j];
    
    for (var k = 0u; k < n; k++) {
        if (k != j) {
            let tk = input_times[start_idx + k];
            let denominator = tj - tk;
            if (abs(denominator) < 1e-10) {
                return 0.0;
            }
            result *= (t - tk) / denominator;
        }
    }
    
    return result;
}
";
