// A low-level module for spawning a child process and figuring out if we are the parent or the
// child using the same binary.
//
// The parent spawns a child process and passes a socket to it. The socket is passed to the child
// via a predefined file descriptor. The child then uses this file descriptor to communicate with
// the parent.
//
// For a process launched using the common binary, it can check if it is a child by checking if the
// [`CANARY_SOCKET_FD`] is valid.
//
// The main goal of this module is to tuck away the low-level machinery like working with libc and
// nix into a single place.

use anyhow::Result;
use cfg_if::cfg_if;
use std::{
    os::{
        fd::{AsRawFd as _, FromRawFd as _, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::process::{Child, Command};
use tracing::trace;

/// A special file descriptor that is used to pass a socket to the child process.
///
/// We pick a high number to avoid conflicts with other file descriptors.
const CANARY_SOCKET_FD: RawFd = 1000;

/// Check whether the given file descriptor is valid.
fn is_valid_fd(fd: RawFd) -> bool {
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Check whether the file descriptor is set to non-blocking mode.
fn is_nonblocking(fd: RawFd) -> bool {
    unsafe { libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK == libc::O_NONBLOCK }
}

/// Check if the file descriptor corresponds to a Unix domain socket.
/// In our case, we're verifying that the socket type is SOCK_STREAM.
fn is_unix_socket(fd: RawFd) -> bool {
    let mut sock_type: libc::c_int = 0;
    let mut type_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            &mut sock_type as *mut _ as *mut _,
            &mut type_len,
        ) == 0
            && sock_type == libc::SOCK_STREAM
    }
}

/// Checks for evidence that this process is a child of a parent process that spawned it.
///
/// Returns a UnixStream if the process is a child, otherwise returns None.
///
/// Panics if called more than once.
pub fn am_spawned() -> Option<UnixStream> {
    static CALLED: AtomicBool = AtomicBool::new(false);
    if CALLED.swap(true, Ordering::SeqCst) {
        // This function should not be called more than once to protect against multiple ownership
        // of the file descriptor.
        panic!();
    }

    if !is_valid_fd(CANARY_SOCKET_FD) {
        return None;
    }

    if !is_unix_socket(CANARY_SOCKET_FD) {
        panic!("not unix socket");
    }

    if !is_nonblocking(CANARY_SOCKET_FD) {
        panic!("non blocking");
    }

    let stream = unsafe {
        // SAFETY:
        // - The file descriptor is valid (checked above with fcntl)
        // - We verified it's actually a Unix domain socket (checked with getsockopt)
        // - This code can only run once due to the TAKEN atomic bool, ensuring we have exclusive
        //   ownership, passing it down into the UnixStream instance.
        // - No other code could have taken ownership as this is the first access (TAKEN was false)
        UnixStream::from_raw_fd(CANARY_SOCKET_FD)
    };
    Some(stream)
}

pub fn spawn_child(workload_dir_path: PathBuf) -> Result<(Child, UnixStream)> {
    let (sock1, sock2) = UnixStream::pair()?;

    // Those sockets are going to be used in tokio and as such they should be both set to
    // non-blocking mode.
    sock1.set_nonblocking(true)?;
    sock2.set_nonblocking(true)?;

    let child = spawn_child_with_sock(sock2.as_raw_fd(), workload_dir_path)?;
    drop(sock2); // Close parent's end in child

    Ok((child, sock1))
}

fn spawn_child_with_sock(socket_fd: RawFd, workload_dir_path: PathBuf) -> Result<Child> {
    trace!(?socket_fd, "Spawning child process");

    // Prepare argv for the child process.
    //
    // Contains only the program binary path and a null terminator.
    cfg_if! {
        if #[cfg(target_os = "linux")] {
            // Nothing beats the simplicity of /proc/self/exe on Linux.
            let program = std::ffi::OsString::from("/proc/self/exe");
        } else {
            let program = std::env::current_exe()?;
        }
    }

    let out_file = std::fs::File::options()
        .create(false)
        .append(true)
        .open(workload_dir_path.join("log.txt"))
        .expect("Log file is expected to be created by the supervisor");
    let mut cmd = Command::new(program);
    cmd.stdout(out_file.try_clone().unwrap());
    cmd.stderr(out_file);
    // Override the PGID of the spawned process. The motivation for this is ^C handling. To handle
    // ^C the shell will send the SIGINT to all processes in the process group. We are handling
    // SIGINT manually in the supervisor process.
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(move || {
            // Duplicate the socket_fd to the CANARY_SOCKET_FD.
            // Close the original socket_fd in the child process.
            libc::dup2(socket_fd, CANARY_SOCKET_FD);
            libc::close(socket_fd);
            Ok(())
        });
    }
    let child = cmd.spawn()?;

    let pid = child
        .id()
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "<killed?>".to_string());
    trace!("spawned child process, pid={pid}");
    Ok(child)
}
