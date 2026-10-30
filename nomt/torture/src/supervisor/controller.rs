use crate::message::{InitOutcome, OpenOutcome};

use super::{comms, config::WorkloadConfiguration};
use anyhow::Result;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{net::UnixStream, process::Child};

/// A controller is responsible for overseeing a single agent process and handle its lifecycle.
pub struct SpawnedAgentController {
    child: Child,
    rr: comms::RequestResponse,
    torn_down: AtomicBool,
    agent_number: usize,
}

// This is a safe-guard to ensure that the [`SpawnedAgentController::teardown`] is called
// properly.
impl Drop for SpawnedAgentController {
    fn drop(&mut self) {
        if self.torn_down.load(Ordering::Relaxed) {
            // The controller was torn down properly, disarm.
            return;
        }
        if std::thread::panicking() {
            // The controller was not torn down properly, but we are panicking.
            eprintln!("controller was not torn down properly");
            return;
        }
        panic!("controller was not torn down properly");
    }
}

impl SpawnedAgentController {
    pub async fn init(
        &mut self,
        workdir: String,
        workload_id: u64,
        trickfs: bool,
    ) -> Result<InitOutcome> {
        let id = format!("agent-{}-{}", workload_id, self.agent_number);
        let response = self
            .rr
            .send_request(crate::message::ToAgent::Init(crate::message::InitPayload {
                id,
                workdir,
                trickfs,
            }))
            .await?;
        match response {
            crate::message::ToSupervisor::InitResponse(outcome) => return Ok(outcome),
            _ => {
                panic!("expected init, unexpected response: {:?}", response);
            }
        }
    }

    pub async fn open(&self, config: &WorkloadConfiguration) -> Result<OpenOutcome> {
        let rollback = if config.is_rollback_enable() {
            Some(config.max_rollback_commits)
        } else {
            None
        };

        let response = self
            .rr
            .send_request(crate::message::ToAgent::Open(crate::message::OpenPayload {
                bitbox_seed: config.bitbox_seed,
                rollback,
                commit_concurrency: config.commit_concurrency,
                io_workers: config.io_workers,
                hashtable_buckets: config.hashtable_buckets,
                warm_up: config.warm_up,
                preallocate_ht: config.preallocate_ht,
                page_cache_size: config.page_cache_size,
                leaf_cache_size: config.leaf_cache_size,
                prepopulate_page_cache: config.prepopulate_page_cache,
                page_cache_upper_levels: config.page_cache_upper_levels,
            }))
            .await?;
        match response {
            crate::message::ToSupervisor::OpenResponse(outcome) => return Ok(outcome),
            _ => {
                panic!("expected open, unexpected response: {:?}", response);
            }
        }
    }

    /// Kills the process, shuts down the comms, and cleans up the resources.
    ///
    /// This returns only when the process is dead and the resources are cleaned up.
    ///
    /// The controller must be torn down manually. Dropping the controller is disallowed. This is
    /// done to control precisely when the agent process is killed.
    pub async fn teardown(mut self) {
        self.torn_down.store(true, Ordering::Relaxed);
        let _ = self.child.kill().await;
    }

    /// Resolves when the agent process exits.
    pub async fn died(&mut self) {
        let _ = self.child.wait().await;
    }

    pub fn rr(&self) -> &comms::RequestResponse {
        &self.rr
    }

    /// Returns the PID of the agent process.
    ///
    /// Returns `None` if the agent is torn down.
    pub fn pid(&self) -> Option<u32> {
        if self.torn_down.load(Ordering::Relaxed) {
            None
        } else {
            self.child.id()
        }
    }
}

/// Spawns an agent process creating a controller.
///
/// The controller is placed in the `place` argument. `place` must be `None` when calling this
/// function.
pub async fn spawn_agent_into(
    place: &mut Option<SpawnedAgentController>,
    output_path: PathBuf,
) -> Result<()> {
    assert!(place.is_none(), "the controller must be empty");

    let (child, sock) = crate::spawn::spawn_child(output_path)?;

    let stream = UnixStream::from_std(sock)?;

    let (rr, task) = comms::run(stream);
    let _ = tokio::spawn(task);

    // Assign a unique ID to the agent.
    static AGENT_COUNT: AtomicUsize = AtomicUsize::new(0);
    let agent_number = AGENT_COUNT.fetch_add(1, Ordering::Relaxed);

    *place = Some(SpawnedAgentController {
        agent_number,
        child,
        rr,
        torn_down: AtomicBool::new(false),
    });
    Ok(())
}
