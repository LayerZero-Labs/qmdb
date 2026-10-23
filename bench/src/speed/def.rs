pub const PAGE_SIZE: usize = 8;
pub const L: usize = 16;

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
