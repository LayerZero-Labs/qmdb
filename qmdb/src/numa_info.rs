use std::io;
use std::process::Command;

pub fn get_node_count() -> io::Result<usize> {
    // Try to get NUMA node count using lscpu on Unix-like systems
    let output = Command::new("lscpu")
        .output()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    
    // Parse the output to find NUMA node count
    for line in output_str.lines() {
        if line.contains("NUMA node(s)") {
            if let Some(count_str) = line.split(':').nth(1) {
                if let Ok(count) = count_str.trim().parse() {
                    return Ok(count);
                }
            }
        }
    }
    
    // Default to 1 if we can't determine NUMA topology
    Ok(1)
}

pub fn get_numa_node_for_cpu(cpu_id: usize) -> io::Result<usize> {
    // Try to get NUMA node for a specific CPU using lscpu on Unix-like systems
    let output = Command::new("lscpu")
        .arg("-p=NODE")
        .output()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    
    // Parse the output to find NUMA node for the CPU
    for (i, line) in output_str.lines().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        if i == cpu_id {
            if let Ok(node) = line.trim().parse() {
                return Ok(node);
            }
        }
    }
    
    // Default to node 0 if we can't determine the NUMA node
    Ok(0)
}

pub fn pin_thread_to_numa_node(node: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use libc::{cpu_set_t, CPU_SET, sched_setaffinity, CPU_ZERO};
        
        unsafe {
            let mut cpuset: cpu_set_t = std::mem::zeroed();
            CPU_ZERO(&mut cpuset);
            
            // Get CPUs for this NUMA node
            let output = Command::new("lscpu")
                .arg("-p=CPU,NODE")
                .output()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                
            let output_str = String::from_utf8_lossy(&output.stdout);
            
            // Parse output and set CPU affinity
            for line in output_str.lines() {
                if line.starts_with('#') {
                    continue;
                }
                let parts: Vec<&str> = line.split(',').collect();
                if parts.len() == 2 {
                    if let (Ok(cpu), Ok(numa_node)) = (parts[0].trim().parse::<usize>(), parts[1].trim().parse::<usize>()) {
                        if numa_node == node {
                            CPU_SET(cpu, &mut cpuset);
                        }
                    }
                }
            }
            
            // Set thread affinity
            let res = sched_setaffinity(0, std::mem::size_of::<cpu_set_t>(), &cpuset);
            if res != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    
    Ok(())
} 