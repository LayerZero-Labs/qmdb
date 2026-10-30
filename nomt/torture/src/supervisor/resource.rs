use std::{
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use rand::{Rng, SeedableRng};

/// 1GiB is the minimum amount of space that can be assigned to a workload.
const MIN_ASSIGNED_SPACE: u64 = 1 * (1 << 30);

/// 5GiB is the maximum amount of memory that can be assigned to a workload
/// which will use both disk and memory.
const MAX_ASSIGNED_MEMORY: u64 = 5 * (1 << 30);

/// 50GiB is the maximum amount of memory that can be assigned to a workload
/// which will use only memory.
const MAX_ASSIGNED_ONLY_MEMORY: u64 = 50 * (1 << 30);

#[derive(Clone, Copy)]
/// Amount of resources assigned to a workload.
/// Resources are disk space and memory.
pub struct AssignedResources {
    pub disk: u64,
    pub memory: u64,
}

/// ResourceAllocator is used to split resources randomly across multiple workloads.
///
/// Resources are Memory and Disk space.
pub struct ResourceAllocator {
    rng: rand_pcg::Pcg64,
    assigned: Vec<(u64, AssignedResources)>,
    max_disk_avail: u64,
    max_memory_avail: u64,
    total_assigned_disk: u64,
    total_assigned_memory: u64,
}

impl ResourceAllocator {
    /// Creates a `ResourceAllocator` given the available disk space at the path of the workdir,
    /// where all workload data will be saved.
    pub fn new(
        workdir_path: PathBuf,
        seed: u64,
        max_disk_occupancy_ratio: u8,
        max_memory_occupancy_ratio: u8,
    ) -> anyhow::Result<Self> {
        let max_disk_occupancy_ratio = max_disk_occupancy_ratio as f64 / 100.0;
        let max_memory_occupancy_ratio = max_memory_occupancy_ratio as f64 / 100.0;
        let (avail_disk, total_disk) = disk_info(&workdir_path);
        let occupied_disk = total_disk - avail_disk;
        let max_disk_occupancy = (total_disk as f64 * max_disk_occupancy_ratio) as u64;
        let Some(max_disk_avail) = max_disk_occupancy.checked_sub(occupied_disk) else {
            anyhow::bail!("Free disk space is less than what was expected to be occupied at most");
        };

        let (avail_memory, total_memory) = mem_info();
        let occupied_memory = total_memory - avail_memory;
        let max_memory_occupancy = (total_memory as f64 * max_memory_occupancy_ratio) as u64;
        let Some(max_memory_avail) = max_memory_occupancy.checked_sub(occupied_memory) else {
            anyhow::bail!("Free memory is less than what was expected to be occupied at most");
        };

        Ok(Self {
            rng: rand_pcg::Pcg64::seed_from_u64(seed),
            assigned: vec![],
            max_memory_avail,
            max_disk_avail,
            total_assigned_disk: 0,
            total_assigned_memory: 0,
        })
    }

    /// It assigns portions of disk space and memory randomly to the specified `workload_id`.
    pub fn alloc(&mut self, workload_id: u64) -> Result<(), ResourceExhaustion> {
        // Assign disk space
        let avail_disk = self.max_disk_avail - self.total_assigned_disk;
        if avail_disk <= MIN_ASSIGNED_SPACE {
            return Err(ResourceExhaustion::Disk);
        }

        // Force to allocate bigger chunk of memory initially.
        let min = std::cmp::min(MIN_ASSIGNED_SPACE, avail_disk / 2);
        let assigned_disk = self.rng.gen_range(min..avail_disk);

        // Assign memory
        let mut avail_memory = self.max_memory_avail - self.total_assigned_memory;
        if avail_memory <= MIN_ASSIGNED_SPACE {
            return Err(ResourceExhaustion::Memory);
        }

        let assigned_disk_ratio = assigned_disk as f64 / avail_disk as f64;
        avail_memory = std::cmp::min(avail_memory, MAX_ASSIGNED_MEMORY);
        let assigned_memory = (avail_memory as f64 * assigned_disk_ratio) as u64;

        self.total_assigned_disk += assigned_disk;
        self.total_assigned_memory += assigned_memory;
        self.assigned.push((
            workload_id,
            AssignedResources {
                disk: assigned_disk,
                memory: assigned_memory,
            },
        ));

        Ok(())
    }

    pub fn alloc_memory(&mut self, workload_id: u64) -> Result<(), ResourceExhaustion> {
        let mut avail_memory = self.max_memory_avail - self.total_assigned_memory;
        if avail_memory <= MIN_ASSIGNED_SPACE {
            return Err(ResourceExhaustion::Memory);
        }

        avail_memory = std::cmp::min(avail_memory, MAX_ASSIGNED_ONLY_MEMORY);
        let assigned_memory = self.rng.gen_range(MIN_ASSIGNED_SPACE..avail_memory);

        self.total_assigned_memory += assigned_memory;
        self.assigned.push((
            workload_id,
            AssignedResources {
                disk: 0,
                memory: assigned_memory,
            },
        ));
        Ok(())
    }

    /// Remove the `workload_id` from tracked ones.
    ///
    /// Panics if the specified workload_id is not present in the tracked ones.
    pub fn free(&mut self, workload_id: u64) {
        let idx = self
            .assigned
            .iter()
            .position(|(id, ..)| *id == workload_id)
            .unwrap();

        let (_, AssignedResources { disk, memory }) = self.assigned.remove(idx);
        self.total_assigned_disk -= disk;
        self.total_assigned_memory -= memory;
    }

    /// Ensures that the workload_dir does not occupy more disk space than the assigned limit
    /// and the same applies to the memory utilization of the specified process_id.
    ///
    /// Return false if the process_id does not refer to an ongoing process.
    ///
    /// Panics if:
    /// 1. workload_id is not present in the tracked ones.
    /// 2. workload_dir_path does not exist.
    pub fn is_exceeding_resources(
        &self,
        workload_id: u64,
        workload_dir_path: &Path,
        process_id: u32,
    ) -> bool {
        let (_, AssignedResources { disk, memory }) = self
            .assigned
            .iter()
            .find(|(id, ..)| *id == workload_id)
            .unwrap();
        is_exceeding_resources(*disk, *memory, workload_dir_path, process_id)
    }

    /// Fetch the amount of assigned resources to the specified `workload_id`.
    ///
    /// Panics if the specified workload_id is not present in the tracked ones.
    pub fn assigned_resources(&self, workload_id: u64) -> AssignedResources {
        self.assigned
            .iter()
            .find(|(id, ..)| *id == workload_id)
            .map(|(_, assigned_resources)| *assigned_resources)
            .unwrap()
    }
}

/// During resource allocation the `ResourceAllocator` could return the following errors.
#[derive(Debug)]
pub enum ResourceExhaustion {
    /// max_disk_occupancy_ratio reached
    Disk,
    /// max_memory_occupancy_ratio reached
    Memory,
}

/// Return the amount of physical memory utilized by the specified pid, or None
/// if the `statm` file associated with the `pid` is not available.
pub fn process_memory_occupied(pid: u32) -> Option<u64> {
    let file_path = format!("/proc/{}/statm", pid);
    let mut statm_file = std::fs::File::open(&file_path).ok()?;
    let format_err = format!("{} not formatted as expected", file_path);
    let mut statm = String::new();
    statm_file.read_to_string(&mut statm).expect(&format_err);
    let occupied_memory = statm
        .split(' ')
        .skip(1)
        .next()
        .map(|value| value.parse::<u64>().expect(&format_err))
        .expect(&format_err);
    Some(occupied_memory)
}

/// Returns the number of available and total memory in bytes.
///
/// Panics if /proc/meminfo does not exist or is not formatted as usual.
fn mem_info() -> (u64, u64) {
    let mut meminfo_file =
        std::fs::File::open("/proc/meminfo").expect("Getting memory information failed");
    let mut meminfo = String::new();
    let format_err = "/proc/meminfo not formatted as expected";
    meminfo_file.read_to_string(&mut meminfo).expect(format_err);
    let find = |name| {
        meminfo
            .split('\n')
            .find(|entry| entry.starts_with(name))
            .and_then(|entry| entry.split(':').nth(1))
            .and_then(|value| value.trim_start().split(' ').nth(0))
            .and_then(|value| value.parse::<u64>().ok())
    };
    // /proc/meminfo shows values in KiB.
    let avail_mem = find("MemAvailable").expect(format_err) * 1024;
    let total_mem = find("MemTotal").expect(format_err) * 1024;
    (avail_mem, total_mem)
}

/// Returns the number of available and total bytes present in the disk where `path`
/// is located.
///
/// Panics if `libc::statvfs` fails.
fn disk_info(path: &Path) -> (u64, u64) {
    let path_cstr = std::ffi::CString::new(path.display().to_string()).unwrap();
    let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path_cstr.as_ptr(), &mut statvfs as *mut libc::statvfs) } < 0 {
        panic!("Getting disk information failed");
    }
    // The casts are necessary because on macOS `f_bavail` and `f_blocks` are u32.
    let avail_disk = statvfs.f_bsize * statvfs.f_bavail as u64;
    let total_disk = statvfs.f_bsize * statvfs.f_blocks as u64;
    (avail_disk, total_disk)
}

/// Ensures that the workload_dir does not occupy more disk space than the assigned limit
/// and the same applies to the memory utilization of the specified process_id.
///
/// Return false if the process_id does not refer to an ongoing process.
///
/// Panics if:
/// 1. workload_dir_path does not exist.
/// 2. process_id does not refer to an ongoing process.
pub fn is_exceeding_resources(
    assigned_disk: u64,
    assigned_memory: u64,
    workload_dir_path: &Path,
    process_id: u32,
) -> bool {
    let Some(used_mem) = process_memory_occupied(process_id) else {
        return true;
    };
    if used_mem >= assigned_memory {
        return true;
    }

    if assigned_disk == 0 {
        return false;
    }

    fn dir_size(path: &Path) -> u64 {
        std::fs::read_dir(path)
            .unwrap()
            .into_iter()
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let entry_metadata = entry.metadata().unwrap();
                if entry_metadata.is_dir() {
                    dir_size(&entry.path())
                } else {
                    entry_metadata.size()
                }
            })
            .sum()
    }

    let used_disk = dir_size(workload_dir_path);
    if used_disk >= assigned_disk {
        return true;
    }

    false
}
