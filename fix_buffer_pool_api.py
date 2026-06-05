import re

# Read the file
with open(r"D:\Development\DSP\splimes\src\gpu\types\interpolator.rs", 'r') as f:
    content = f.read()

# Fix the f64 acquire calls - need to add size and label parameters
# Pattern: interpolator.buffer_pool.acquire(PooledBufferType::TargetTimes)?
# Replacement: interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Storage, Some("Target Times Buffer"))?.0

# Find f64 batch section
f64_start = content.find("for target_batch in target_times.chunks(max_elements_per_batch) {", content.find("pub fn interpolate_f64_static"))
f64_end = content.find("Ok(all_results)", content.find("pub fn interpolate_f64_static"))

# Extract f64 batch section
f64_section = content[f64_start:f64_end]

# Replace the three acquire calls in f64
f64_new_section = f64_section.replace(
    'let target_times_buffer = interpolator.buffer_pool.acquire(PooledBufferType::TargetTimes)?;',
    'let (target_times_buffer, _) = interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Storage, Some("Target Times Buffer"))?;'
)

f64_new_section = f64_new_section.replace(
    'let output_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Output)?;',
    'let (output_buffer, _) = interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Storage, Some("Output Buffer"))?;'
)

f64_new_section = f64_new_section.replace(
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;',
    'let (staging_buffer, _) = interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Staging, Some("Staging Buffer"))?;'
)

# Update the release calls to not expect Result (returns void)
f64_new_section = f64_new_section.replace(
    'interpolator.buffer_pool.release(target_times_buffer)?;',
    'interpolator.buffer_pool.release(target_times_buffer);'
)

f64_new_section = f64_new_section.replace(
    'interpolator.buffer_pool.release(output_buffer)?;',
    'interpolator.buffer_pool.release(output_buffer);'
)

f64_new_section = f64_new_section.replace(
    'interpolator.buffer_pool.release(staging_buffer)?;',
    'interpolator.buffer_pool.release(staging_buffer);'
)

content = content[:f64_start] + f64_new_section + content[f64_end:]

# Now fix f32
f32_start = content.find("for target_batch in target_times.chunks(max_elements_per_batch) {", content.find("pub fn interpolate_f32_static"))
f32_end = content.find("Ok(all_results)", content.find("pub fn interpolate_f32_static"))

f32_section = content[f32_start:f32_end]

f32_new_section = f32_section.replace(
    'let target_times_buffer = interpolator.buffer_pool.acquire(PooledBufferType::TargetTimes)?;',
    'let (target_times_buffer, _) = interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Storage, Some("Target Times Buffer"))?;'
)

f32_new_section = f32_new_section.replace(
    'let output_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Output)?;',
    'let (output_buffer, _) = interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Storage, Some("Output Buffer"))?;'
)

f32_new_section = f32_new_section.replace(
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;',
    'let (staging_buffer, _) = interpolator.buffer_pool.acquire(output_size as u64, PooledBufferType::Staging, Some("Staging Buffer"))?;'
)

f32_new_section = f32_new_section.replace(
    'interpolator.buffer_pool.release(target_times_buffer)?;',
    'interpolator.buffer_pool.release(target_times_buffer);'
)

f32_new_section = f32_new_section.replace(
    'interpolator.buffer_pool.release(output_buffer)?;',
    'interpolator.buffer_pool.release(output_buffer);'
)

f32_new_section = f32_new_section.replace(
    'interpolator.buffer_pool.release(staging_buffer)?;',
    'interpolator.buffer_pool.release(staging_buffer);'
)

content = content[:f32_start] + f32_new_section + content[f32_end:]

# Write back
with open(r"D:\Development\DSP\splimes\src\gpu\types\interpolator.rs", 'w') as f:
    f.write(content)

print("Fixed buffer pool API calls")
