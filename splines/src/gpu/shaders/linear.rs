/// GPU compute shader for linear interpolation
pub const LINEAR_INTERPOLATION_SHADER: &str = r"
@group(0) @binding(0) var<storage, read> input_times: array<f32>;
@group(0) @binding(1) var<storage, read> input_values: array<f32>;
@group(0) @binding(2) var<storage, read> target_times: array<f32>;
@group(0) @binding(3) var<storage, read_write> output_values: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let index = global_id.x;
    let target_count = arrayLength(&target_times);
    
    if (index >= target_count) {
        return;
    }
    
    let target_time = target_times[index];
    let input_count = arrayLength(&input_times);
    
    if (input_count < 2u) {
        output_values[index] = 0.0;
        return;
    }
    
    // Handle extrapolation cases
    if (target_time <= input_times[0]) {
        // Backward extrapolation using first two points
        let dt = input_times[1] - input_times[0];
        if (abs(dt) < 0.001) {
            output_values[index] = input_values[0];
            return;
        }
        let slope = (input_values[1] - input_values[0]) / dt;
        output_values[index] = input_values[0] + slope * (target_time - input_times[0]);
        return;
    }
    
    if (target_time >= input_times[input_count - 1u]) {
        // Forward extrapolation using last two points
        let dt = input_times[input_count - 1u] - input_times[input_count - 2u];
        if (abs(dt) < 0.001) {
            output_values[index] = input_values[input_count - 1u];
            return;
        }
        let slope = (input_values[input_count - 1u] - input_values[input_count - 2u]) / dt;
        output_values[index] = input_values[input_count - 1u] + slope * (target_time - input_times[input_count - 1u]);
        return;
    }
    
    // Binary search for the correct segment
    var left = 0u;
    var right = input_count - 1u;
    
    while (left < right - 1u) {
        let mid = (left + right) / 2u;
        if (input_times[mid] <= target_time) {
            left = mid;
        } else {
            right = mid;
        }
    }
    
    // Linear interpolation between left and right
    let t0 = input_times[left];
    let t1 = input_times[right];
    let v0 = input_values[left];
    let v1 = input_values[right];
    
    let dt = t1 - t0;
    if (abs(dt) < 0.001) {
        output_values[index] = v0;
        return;
    }
    
    let alpha = (target_time - t0) / dt;
    let clamped_alpha = clamp(alpha, 0.0, 1.0);
    output_values[index] = v0 + clamped_alpha * (v1 - v0);
}
";
