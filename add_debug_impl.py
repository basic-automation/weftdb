# Read the file
with open(r"D:\Development\DSP\splimes\src\gpu\buffer_pool.rs", 'r') as f:
    content = f.read()

# Find where to insert the Debug impl
# Look for "impl BufferPool {" and insert before it
impl_pos = content.find("impl BufferPool {")

debug_impl = """impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool")
            .field("config", &self.config)
            .field("total_allocated", &self.total_allocated.load(std::sync::atomic::Ordering::Relaxed))
            .field("total_allocations", &self.total_allocations.load(std::sync::atomic::Ordering::Relaxed))
            .field("total_reuses", &self.total_reuses.load(std::sync::atomic::Ordering::Relaxed))
            .finish()
    }
}

"""

# Insert before impl BufferPool
content = content[:impl_pos] + debug_impl + content[impl_pos:]

with open(r"D:\Development\DSP\splimes\src\gpu\buffer_pool.rs", 'w') as f:
    f.write(content)

print("Added Debug impl for BufferPool")
