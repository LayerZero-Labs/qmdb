// ============================================================================
// When `instruction_meter` feature is enabled, use full implementation
// ============================================================================
#[cfg(feature = "instruction_meter")]
mod enabled {
    extern crate std;

    use alloc::string::String;

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    use perf_event::{events::Hardware, Builder, Counter};

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::time::Instant;

    use std::eprintln;
    use std::println;

    /// A small helper to measure QEVM instruction count / execution time / cycles.
    ///
    /// - **Linux ARM64**: Uses `perf_event` to count hardware instructions.
    /// - **macOS ARM64 (with std)**: Uses `std::time::Instant` to measure wall-clock duration.
    /// - **Others**: Logs a "not supported" message.
    ///
    /// **Note**: This struct only outputs results when the `instruction_meter` feature is enabled.
    /// By default, all methods are no-ops.
    ///
    /// Usage:
    /// ```ignore
    /// let mut meter = InstructionMeter::new("execute_tx".to_string());
    /// // ... run logic ...
    /// meter.end();
    /// ```
    pub struct InstructionMeter {
        name: String,

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        counter: Option<Counter>,

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        start_time: Option<Instant>,
    }

    impl InstructionMeter {
        pub fn new(name: String) -> Self {
            let mut r = Self {
                name,
                #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
                counter: None,
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                start_time: None,
            };
            r.start();
            r
        }

        /// Start metering.
        fn start(&mut self) {
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            {
                if self.counter.is_some() {
                    return;
                }

                let result = Builder::new().kind(Hardware::INSTRUCTIONS).build();

                match result {
                    Ok(mut counter) => {
                        if let Err(e) = counter.enable() {
                            eprintln!("InstructionMeter [{}] failed enable: {:?}", self.name, e);
                        } else {
                            self.counter = Some(counter);
                        }
                    }
                    Err(e) => {
                        eprintln!("InstructionMeter [{}] failed create: {:?}", self.name, e);
                    }
                }
            }

            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                self.start_time = Some(Instant::now());
            }
        }

        /// Stop metering and log the result.
        pub fn end(&mut self) {
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            {
                let mut counter = self.counter.take().unwrap();
                let _ = counter.disable(); // ignore error on disable
                match counter.read() {
                    Ok(count) => {
                        println!("InstructionMeter [{}] instructions = {}", self.name, count);
                    }
                    Err(e) => {
                        eprintln!("InstructionMeter [{}] failed read: {:?}", self.name, e);
                    }
                }
            }

            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let start = self.start_time.take().unwrap();
                let duration = start.elapsed();
                println!("InstructionMeter [{}] duration = {:?}", self.name, duration);
            }

            #[cfg(not(any(
                all(target_os = "linux", target_arch = "aarch64"),
                all(target_os = "macos", target_arch = "aarch64"),
            )))]
            {
                println!(
                    "InstructionMeter [{}] metering not supported on this platform",
                    self.name
                );
            }
        }
    }
}

// ============================================================================
// When `instruction_meter` feature is disabled, use no-op implementation (zero overhead)
// ============================================================================
#[cfg(not(feature = "instruction_meter"))]
mod disabled {
    use alloc::string::String;

    /// A no-op InstructionMeter when the `instruction_meter` feature is disabled.
    ///
    /// All methods are empty and should be completely optimized away by the compiler.
    pub struct InstructionMeter {
        _name: String,
    }

    impl InstructionMeter {
        #[inline(always)]
        pub fn new(name: String) -> Self {
            Self { _name: name }
        }

        #[inline(always)]
        pub fn end(&mut self) {
            // No-op
        }
    }
}

// Re-export the appropriate implementation
#[cfg(feature = "instruction_meter")]
pub use enabled::InstructionMeter;

#[cfg(not(feature = "instruction_meter"))]
pub use disabled::InstructionMeter;

#[cfg(test)]
mod tests {
    use super::InstructionMeter;
    use alloc::string::ToString;

    #[test]
    fn test_instruction_meter_run() {
        // Ensure it runs without panic on any platform.
        // On supported platforms (ARM Linux/Mac), check logs for output.
        let mut meter = InstructionMeter::new("test_scope".to_string());

        // Do some work
        let mut _acc = 0u64;
        for i in 0..1000 {
            _acc = _acc.wrapping_add(i);
        }

        // Should log output
        meter.end();
    }
}
