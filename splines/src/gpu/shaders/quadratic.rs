/// GPU compute shader for quadratic interpolation
pub const QUADRATIC_INTERPOLATION_SHADER: &str = r"
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
    
    if (input_count < 3u) {
        // Fall back to linear interpolation for insufficient points
        if (input_count < 2u) {
            output_values[index] = 0.0;
            return;
        }
        
        // Linear interpolation fallback
        let dt = input_times[1] - input_times[0];
        if (abs(dt) < 0.001) {
            output_values[index] = input_values[0];
            return;
        }
        let alpha = (target_time - input_times[0]) / dt;
        output_values[index] = input_values[0] + alpha * (input_values[1] - input_values[0]);
        return;
    }
    
    // Handle extrapolation cases using quadratic fit from edge points
    if (target_time <= input_times[0]) {
        // Backward extrapolation using first three points
        let t0 = input_times[0];
        let t1 = input_times[1];
        let t2 = input_times[2];
        let v0 = input_values[0];
        let v1 = input_values[1];
        let v2 = input_values[2];
        
        output_values[index] = quadratic_interpolate(target_time, t0, t1, t2, v0, v1, v2);
        return;
    }
    
    if (target_time >= input_times[input_count - 1u]) {
        // Forward extrapolation using last three points
        let t0 = input_times[input_count - 3u];
        let t1 = input_times[input_count - 2u];
        let t2 = input_times[input_count - 1u];
        let v0 = input_values[input_count - 3u];
        let v1 = input_values[input_count - 2u];
        let v2 = input_values[input_count - 1u];
        
        output_values[index] = quadratic_interpolate(target_time, t0, t1, t2, v0, v1, v2);
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
    
    // Choose three points for quadratic interpolation
    var i0: u32;
    var i1: u32;
    var i2: u32;
    
    if (left == 0u) {
        // Use first three points
        i0 = 0u;
        i1 = 1u;
        i2 = 2u;
    } else if (right == input_count - 1u) {
        // Use last three points
        i0 = input_count - 3u;
        i1 = input_count - 2u;
        i2 = input_count - 1u;
    } else {
        // Use centered three points
        i0 = left - 1u;
        i1 = left;
        i2 = right;
    }
    
    let t0 = input_times[i0];
    let t1 = input_times[i1];
    let t2 = input_times[i2];
    let v0 = input_values[i0];
    let v1 = input_values[i1];
    let v2 = input_values[i2];
    
    output_values[index] = quadratic_interpolate(target_time, t0, t1, t2, v0, v1, v2);
}

fn quadratic_interpolate(t: f32, t0: f32, t1: f32, t2: f32, v0: f32, v1: f32, v2: f32) -> f32 {
    // Lagrange interpolation formula for quadratic
    let dt01 = t0 - t1;
    let dt02 = t0 - t2;
    let dt12 = t1 - t2;
    
    // Check for degenerate cases
    if (abs(dt01) < 0.001 || abs(dt02) < 0.001 || abs(dt12) < 0.001) {
        // Fall back to nearest value
        let d0 = abs(t - t0);
        let d1 = abs(t - t1);
        let d2 = abs(t - t2);
        
        if (d0 <= d1 && d0 <= d2) {
            return v0;
        } else if (d1 <= d2) {
            return v1;
        } else {
            return v2;
        }
    }
    
    // Lagrange basis functions
    let l0 = ((t - t1) * (t - t2)) / (dt01 * dt02);
    let l1 = ((t - t0) * (t - t2)) / (-dt01 * dt12);
    let l2 = ((t - t0) * (t - t1)) / (dt02 * (-dt12));
    
    return l0 * v0 + l1 * v1 + l2 * v2;
}
";
