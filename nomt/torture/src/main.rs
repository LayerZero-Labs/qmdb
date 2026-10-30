use anyhow::Result;
use tokio::net::UnixStream;

mod agent;
mod logging;
mod message;
mod panic;
mod spawn;
mod supervisor;

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(chan) = spawn::am_spawned() {
        let chan = UnixStream::from_std(chan)?;
        agent::run(chan).await?;
    } else {
        supervisor::run().await?;
    }
    Ok(())
}
