import re

# Read the file
with open(r"D:\Development\DSP\splimes\src\gpu\types\interpolator.rs", 'r') as f:
    content = f.read()

# Define the f64 batch loop pattern and replacement
f64_pattern = r"(for target_batch in target_times\.chunks\(max_elements_per_batch\) \{)\s+(let output_size = std::mem::size_of_val\(target_batch\);)\s+(let target_times_buffer = interpolator\.device\.create_buffer_init\(&BufferInitDescriptor \{ label: Some\(\"Target Times Buffer\"\), contents: cast_slice\(target_batch\), usage: BufferUsages::STORAGE \}\);)\s+(let output_buffer = interpolator\.device\.create_buffer\(&wgpu::BufferDescriptor \{ label: Some\(\"Output Buffer\"\), size: output_size as u64, usage: BufferUsages::STORAGE \| BufferUsages::COPY_SRC, mapped_at_creation: false \}\);)\s+(let staging_buffer = interpolator\.device\.create_buffer\(&wgpu::BufferDescriptor \{ label: Some\(\"Staging Buffer\"\), size: output_size as u64, usage: BufferUsages::COPY_DST \| BufferUsages::MAP_READ, mapped_at_creation: false \}\);)"

f64_replacement = r"""\1
			let output_size = std::mem::size_of_val(target_batch);
			let target_times_buffer = interpolator.buffer_pool.acquire(PooledBufferType::TargetTimes)?;
			let output_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Output)?;
			let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;
			interpolator.queue.write_buffer(&target_times_buffer, 0, cast_slice(target_batch));"""

# First, let's find and replace the f64 batch loop section
# More precise approach: find the exact section and replace

f64_batch_start = content.find("pub fn interpolate_f64_static")
f64_for_loop = content.find("for target_batch in target_times.chunks(max_elements_per_batch) {", f64_batch_start)
f64_batch_end = content.find("all_results.extend(batch_results);", f64_batch_start)
f64_batch_end = content.find("\n\t\t}", f64_batch_end)

# Find the exact section to replace in f64
f64_section_start = f64_for_loop
f64_section = content[f64_section_start:f64_batch_end + 3]

# Replace the buffer creation calls in f64
f64_new_section = f64_section
# Replace 3 create_buffer_init and create_buffer calls
f64_new_section = f64_new_section.replace(
    'let target_times_buffer = interpolator.device.create_buffer_init(&BufferInitDescriptor { label: Some("Target Times Buffer"), contents: cast_slice(target_batch), usage: BufferUsages::STORAGE });',
    'let target_times_buffer = interpolator.buffer_pool.acquire(PooledBufferType::TargetTimes)?;'
)

f64_new_section = f64_new_section.replace(
    'let output_buffer = interpolator.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Output Buffer"), size: output_size as u64, usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC, mapped_at_creation: false });',
    'let output_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Output)?;'
)

f64_new_section = f64_new_section.replace(
    'let staging_buffer = interpolator.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Staging Buffer"), size: output_size as u64, usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ, mapped_at_creation: false });',
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;'
)

# Add write_buffer call after the buffer acquisition
f64_new_section = f64_new_section.replace(
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;\n',
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;\n\t\t\tinterpolator.queue.write_buffer(&target_times_buffer, 0, cast_slice(target_batch));\n'
)

# Remove unmap() call from f64
f64_new_section = f64_new_section.replace(
    'staging_buffer.unmap();\n\t\t\tall_results.extend(batch_results);',
    'all_results.extend(batch_results);\n\t\t\tinterpolator.buffer_pool.release(target_times_buffer)?;\n\t\t\tinterpolator.buffer_pool.release(output_buffer)?;\n\t\t\tinterpolator.buffer_pool.release(staging_buffer)?;'
)

# Replace in content
content = content[:f64_section_start] + f64_new_section + content[f64_batch_end + 3:]

# Now do the same for f32
f32_batch_start = content.find("pub fn interpolate_f32_static")
f32_for_loop = content.find("for target_batch in target_times.chunks(max_elements_per_batch) {", f32_batch_start)
f32_batch_end = content.find("all_results.extend(batch_results);", f32_batch_start)
f32_batch_end = content.find("\n\t\t}", f32_batch_end)

f32_section_start = f32_for_loop
f32_section = content[f32_section_start:f32_batch_end + 3]

f32_new_section = f32_section
f32_new_section = f32_new_section.replace(
    'let target_times_buffer = interpolator.device.create_buffer_init(&BufferInitDescriptor { label: Some("Target Times Buffer"), contents: cast_slice(target_batch), usage: BufferUsages::STORAGE });',
    'let target_times_buffer = interpolator.buffer_pool.acquire(PooledBufferType::TargetTimes)?;'
)

f32_new_section = f32_new_section.replace(
    'let output_buffer = interpolator.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Output Buffer"), size: output_size as u64, usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC, mapped_at_creation: false });',
    'let output_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Output)?;'
)

f32_new_section = f32_new_section.replace(
    'let staging_buffer = interpolator.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Staging Buffer"), size: output_size as u64, usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ, mapped_at_creation: false });',
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;'
)

f32_new_section = f32_new_section.replace(
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;\n',
    'let staging_buffer = interpolator.buffer_pool.acquire(PooledBufferType::Staging)?;\n\t\t\tinterpolator.queue.write_buffer(&target_times_buffer, 0, cast_slice(target_batch));\n'
)

f32_new_section = f32_new_section.replace(
    'staging_buffer.unmap();\n\t\t\tall_results.extend(batch_results);',
    'all_results.extend(batch_results);\n\t\t\tinterpolator.buffer_pool.release(target_times_buffer)?;\n\t\t\tinterpolator.buffer_pool.release(output_buffer)?;\n\t\t\tinterpolator.buffer_pool.release(staging_buffer)?;'
)

content = content[:f32_section_start] + f32_new_section + content[f32_batch_end + 3:]

# Write back
with open(r"D:\Development\DSP\splimes\src\gpu\types\interpolator.rs", 'w') as f:
    f.write(content)

print("Successfully updated interpolator.rs to use buffer pool")
