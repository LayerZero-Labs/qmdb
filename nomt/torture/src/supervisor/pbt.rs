//! Collection of process backtraces.
//!
//! This uses the grug-brain developer approach: just invoke the LLDB or GDB to get the backtrace.

use futures::future::join3;
use std::{path::Path, time::Duration};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
    time::timeout,
};
use which::which;

pub async fn collect_process_backtrace(filename: &Path, pid: u32) -> anyhow::Result<()> {
    // Determine which debugger tool to use.
    let command_str = if which("lldb").is_ok() {
        lldb(pid)
    } else if which("gdb").is_ok() {
        gdb(pid)
    } else {
        anyhow::bail!("no lldb or gdb in PATH")
    };

    // Run the command using a shell
    // Spawn the command using a shell so that we have a Child handle.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command_str)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let mut stdout_pipe = child.stdout.take().expect("stdout pipe");
    let mut stderr_pipe = child.stderr.take().expect("stderr pipe");

    async fn read_pipe(pipe: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<String> {
        let mut reader = tokio::io::BufReader::new(pipe);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await?;
        let stdout = String::from_utf8(buf)?;
        Ok(stdout)
    }

    let stdout_fut = read_pipe(&mut stdout_pipe);
    let stderr_fut = read_pipe(&mut stderr_pipe);

    let (exit_code, stdout, stderr) = match timeout(
        Duration::from_secs(5),
        join3(child.wait(), stdout_fut, stderr_fut),
    )
    .await
    {
        Ok(v) => v,
        Err(_) => {
            // Timed out.
            //
            // Do the best-effort attempt at killing the child process.
            //
            // FIXME: Ideally we kill not just the child process but the entire process group.
            tokio::spawn(async move { child.kill().await });
            anyhow::bail!("Debugger command timed out after 5 seconds");
        }
    };

    let exit_code = exit_code?;
    let stderr = stderr?;
    let stdout = stdout?;

    if !exit_code.success() {
        anyhow::bail!("command '{}' failed: {}", command_str, stderr);
    }

    // Write the backtrace into the file specified by filename.
    fs::write(&filename, &stdout).await?;

    Ok(())
}

/// Generate the lldb command for obtaining the backtrace.
fn lldb(pid: u32) -> String {
    format!(
        "lldb -p {} -o \"thread backtrace all\" -o \"detach\" -o \"quit\"",
        pid
    )
}

/// Generate the gdb command for obtaining the backtrace.
fn gdb(pid: u32) -> String {
    format!(
        "gdb -p {} -batch -ex \"thread apply all bt\" -ex \"detach\" -ex \"quit\"",
        pid
    )
}
