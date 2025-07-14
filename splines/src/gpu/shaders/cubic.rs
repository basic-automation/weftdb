/// GPU compute shader for cubic interpolation
pub const CUBIC_INTERPOLATION_SHADER: &str = r"
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
    
    if (input_count < 4u) {
        // Fall back to lower order interpolation
        if (input_count < 2u) {
            output_values[index] = 0.0;
            return;
        }
        
        if (input_count == 2u) {
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
        
        // Quadratic interpolation fallback for 3 points
        let t0 = input_times[0];
        let t1 = input_times[1];
        let t2 = input_times[2];
        let v0 = input_values[0];
        let v1 = input_values[1];
        let v2 = input_values[2];
        
        output_values[index] = quadratic_interpolate(target_time, t0, t1, t2, v0, v1, v2);
        return;
    }
    
    // Handle extrapolation cases using cubic fit from edge points
    if (target_time <= input_times[0]) {
        // Backward extrapolation using first four points
        let t0 = input_times[0];
        let t1 = input_times[1];
        let t2 = input_times[2];
        let t3 = input_times[3];
        let v0 = input_values[0];
        let v1 = input_values[1];
        let v2 = input_values[2];
        let v3 = input_values[3];
        
        output_values[index] = cubic_interpolate(target_time, t0, t1, t2, t3, v0, v1, v2, v3);
        return;
    }
    
    if (target_time >= input_times[input_count - 1u]) {
        // Forward extrapolation using last four points
        let t0 = input_times[input_count - 4u];
        let t1 = input_times[input_count - 3u];
        let t2 = input_times[input_count - 2u];
        let t3 = input_times[input_count - 1u];
        let v0 = input_values[input_count - 4u];
        let v1 = input_values[input_count - 3u];
        let v2 = input_values[input_count - 2u];
        let v3 = input_values[input_count - 1u];
        
        output_values[index] = cubic_interpolate(target_time, t0, t1, t2, t3, v0, v1, v2, v3);
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
    
    // Choose four points for cubic interpolation
    var i0: u32;
    var i1: u32;
    var i2: u32;
    var i3: u32;
    
    if (left == 0u) {
        // Use first four points
        i0 = 0u;
        i1 = 1u;
        i2 = 2u;
        i3 = 3u;
    } else if (left == 1u) {
        // Use points 0,1,2,3 for better centering
        i0 = 0u;
        i1 = 1u;
        i2 = 2u;
        i3 = 3u;
    } else if (right >= input_count - 2u) {
        // Use last four points
        i0 = input_count - 4u;
        i1 = input_count - 3u;
        i2 = input_count - 2u;
        i3 = input_count - 1u;
    } else {
        // Use centered four points
        i0 = left - 1u;
        i1 = left;
        i2 = right;
        i3 = right + 1u;
    }
    
    let t0 = input_times[i0];
    let t1 = input_times[i1];
    let t2 = input_times[i2];
    let t3 = input_times[i3];
    let v0 = input_values[i0];
    let v1 = input_values[i1];
    let v2 = input_values[i2];
    let v3 = input_values[i3];
    
    output_values[index] = cubic_interpolate(target_time, t0, t1, t2, t3, v0, v1, v2, v3);
}

fn cubic_interpolate(t: f32, t0: f32, t1: f32, t2: f32, t3: f32, v0: f32, v1: f32, v2: f32, v3: f32) -> f32 {
    // Lagrange interpolation formula for cubic
    let dt01 = t0 - t1;
    let dt02 = t0 - t2;
    let dt03 = t0 - t3;
    let dt12 = t1 - t2;
    let dt13 = t1 - t3;
    let dt23 = t2 - t3;
    
    // Check for degenerate cases
    if (abs(dt01) < 0.001 || abs(dt02) < 0.001 || abs(dt03) < 0.001 || 
        abs(dt12) < 0.001 || abs(dt13) < 0.001 || abs(dt23) < 0.001) {
        // Fall back to nearest value
        let d0 = abs(t - t0);
        let d1 = abs(t - t1);
        let d2 = abs(t - t2);
        let d3 = abs(t - t3);
        
        if (d0 <= d1 && d0 <= d2 && d0 <= d3) {
            return v0;
        } else if (d1 <= d2 && d1 <= d3) {
            return v1;
        } else if (d2 <= d3) {
            return v2;
        } else {
            return v3;
        }
    }
    
    // Lagrange basis functions
    let l0 = ((t - t1) * (t - t2) * (t - t3)) / (dt01 * dt02 * dt03);
    let l1 = ((t - t0) * (t - t2) * (t - t3)) / (-dt01 * dt12 * dt13);
    let l2 = ((t - t0) * (t - t1) * (t - t3)) / (dt02 * (-dt12) * dt23);
    let l3 = ((t - t0) * (t - t1) * (t - t2)) / (-dt03 * dt13 * (-dt23));
    
    return l0 * v0 + l1 * v1 + l2 * v2 + l3 * v3;
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
