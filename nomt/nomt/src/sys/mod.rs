//! Platform-specific code.
//!
//! At the moment we only target Linux and macOS.

cfg_if::cfg_if! {
    if #[cfg(target_os = "linux")] {
        pub mod linux;
        pub mod unix;
    } else if #[cfg(target_os = "macos")] {
        pub mod macos;
        pub mod unix;
    }
}
