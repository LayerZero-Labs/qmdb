//! The supervisor part. Spawns and manages agents. Assigns work to agents.

use std::{
    path::PathBuf,
    process::exit,
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use clap::Parser;
use cli::{Cli, RunParams, SwarmParams};
use resource::{AssignedResources, ResourceAllocator, ResourceExhaustion};
use tempfile::TempDir;
use tokio::{
    signal::unix::{signal, SignalKind},
    task::{self, JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument::WithSubscriber, warn};

use crate::logging;
use workload::Workload;

mod cli;
mod comms;
mod config;
mod controller;
mod pbt;
mod resource;
mod swarm;
mod workload;

/// The entrypoint for the supervisor part of the program.
///
/// This is not expected to return explicitly unless there was an error.
pub async fn run() -> Result<()> {
    logging::init_supervisor();

    // Create a cancellation token and spawn the control loop task passing the token to it and
    // wait until the control loop task finishes or the user interrupts it.
    let ct = CancellationToken::new();

    let cli = Cli::parse();
    let swarm_params = match cli.command {
        cli::Commands::Swarm(swarm_params) => swarm_params,
        cli::Commands::Run(run_params) => return run_single_workload(ct, run_params).await,
    };
    let seed = rand::random();

    let control_loop_jh = task::spawn(control_loop(ct.clone(), seed, swarm_params));
    match join_interruptable(control_loop_jh, ct).await {
        ExitReason::Finished => {
            exit(0);
        }
        ExitReason::Error(error) => {
            error!("Error occurred in supervisor's control_loop: {:?}", error);
            exit(1);
        }
        ExitReason::Panic(panic_box) => {
            // The `Any` payload from a panic
            let panic_string = crate::panic::panic_to_string(
                "Panic occurred in supervisor's control_loop:",
                panic_box,
            );
            error!("{}", panic_string);
            exit(2);
        }
        ExitReason::Interrupted => {
            // The exit code is 128 + 2 because in UNIX, signals are mapped to exit codes by adding
            // 128 to the signal number. SIGINT is signal number 2, so the exit code is:
            //
            //     128 + 2 = 130
            //
            // This is what `cargo test` does.
            exit(130);
        }
    }
}

enum ExitReason {
    /// The `control_loop` task finished successfully.
    Finished,
    /// The `control_loop` task returned an error.
    Error(anyhow::Error),
    /// Similiar to `Error` but the error is a panic.
    Panic(Box<dyn std::any::Any + Send + 'static>),
    /// The `control_loop` task was interrupted by the user.
    Interrupted,
}

/// Wait for the control loop to finish or for the user to interrupt it. Returns the reason for the
/// exit.
///
/// The handling the ^C signal is done as follows:
///
/// 1. First ^C emits the `CancellationToken::cancel` call.
/// 2. Second ^C aborst the control loop task.
async fn join_interruptable(
    mut control_loop_jh: JoinHandle<Result<()>>,
    ct: CancellationToken,
) -> ExitReason {
    // UNWRAP: we don't expect this to fail.
    //         If that happens then something is terribly wrong anyway.
    let mut sigint = signal(SignalKind::interrupt()).unwrap();

    // Either catch the first ^C signal or wait until the succesful completion of the control loop.
    tokio::select! {
        _ = sigint.recv() => {
            info!("Received ^C...");
            ct.cancel();
        }
        res = &mut control_loop_jh => {
            // Control loop ended before receiving ^C.
            match res {
                Ok(Ok(())) => return ExitReason::Finished,
                Ok(Err(e)) => return ExitReason::Error(e),
                Err(join_err) => {
                    // UNWRAP: the task cannot be cancelled because the only place it can get
                    // cancelled is here and we don't cancel it. So it must be a panic.
                    let panic_box = join_err.try_into_panic().unwrap();
                    return ExitReason::Panic(panic_box);
                },
            }
        }
    }

    // At this point we received the first ^C signal and the control loop is still running.
    tokio::select! {
        _ = sigint.recv() => {
            control_loop_jh.abort();
            return ExitReason::Interrupted;
        }
        res = &mut control_loop_jh => {
            // Control loop ended before receiving the second ^C.
            match res {
                Ok(Ok(())) => {
                    // We received the first interrupt meaning that the control loop was cancelled.
                    return ExitReason::Interrupted
                },
                Ok(Err(e)) => return ExitReason::Error(e),
                Err(join_err) => {
                    // UNWRAP: the task cannot be cancelled because the only place it can get
                    // cancelled is here and we don't cancel it. So it must be a panic.
                    let panic_box = join_err.try_into_panic().unwrap();
                    return ExitReason::Panic(panic_box);
                },
            }
        }
    }
}

#[derive(Debug)]
pub struct InvestigationFlag {
    /// Seed used to generate the workload.
    seed: u64,
    /// Amount of disk, in bytes, that was assigned to the workload.
    assigned_disk: u64,
    /// Amount of memory, in bytes, that was assigned to the workload.
    assigned_memory: u64,
    workload_id: u64,
    /// The directory the agent was working in.
    workdir: PathBuf,
    /// The reason for flagging.
    reason: anyhow::Error,
}

fn init_workload_dir(workdir_path: PathBuf, workload_id: u64) -> TempDir {
    let mut workload_dir_builder = tempfile::Builder::new();
    workload_dir_builder.prefix("torture-");
    let suffix = format!("-workload-{}", workload_id);
    workload_dir_builder.suffix(&suffix);
    workload_dir_builder
        .tempdir_in(workdir_path)
        .expect("Failed to create a temp dir")
}

/// Run the workload until either it either finishes, errors or gets cancelled.
///
/// Returns `None` if the investigation is not required (i.e. cancelled or succeeded), otherwise,
/// returns the investigation report.
async fn run_workload(
    cancel_token: CancellationToken,
    seed: u64,
    workload_id: u64,
    mut workload: Workload,
) -> Result<Option<InvestigationFlag>> {
    let workload_dir_path = workload.workload_dir_path();
    let AssignedResources { disk, memory } = workload.assigned_resources();
    let result = workload
        .run(cancel_token)
        .with_subscriber(logging::workload_subscriber(&workload_dir_path))
        .await;

    match result {
        Ok(()) => Ok(None),
        Err(err) => Ok(Some(InvestigationFlag {
            seed,
            workload_id,
            assigned_disk: disk,
            assigned_memory: memory,
            // `TempDir::into_path` persists the TempDir to disk.
            workdir: workload.into_workload_dir().into_path(),
            reason: err,
        })),
    }
}

fn print_flag(flag: &InvestigationFlag) {
    warn!(
        "Flagged for investigation:\n  seed={seed}\n  assigned_disk={assigned_disk}\n  \
         assigned_memory={assigned_memory}\n  workload_id={workload_id}\n  workdir={workdir}\n  \
         reason={reason}",
        seed = flag.seed,
        assigned_disk = flag.assigned_disk,
        assigned_memory = flag.assigned_memory,
        workload_id = flag.workload_id,
        workdir = flag.workdir.display(),
        reason = flag.reason,
    );
}

const NON_DETERMINISM_DISCLAIMER: &str = "torture is a non-deterministic fuzzer.\nThe workload \
    generated by this tool is not guaranteed to be reproducible, but specifying the seed may \
    improve the chances of reproducing the issue.";

/// Run the control loop creating and tearing down the agents.
///
/// `cancel_token` is used to gracefully shutdown the supervisor.
async fn control_loop(
    cancel_token: CancellationToken,
    seed: u64,
    mut swarm_params: SwarmParams,
) -> Result<()> {
    info!("Starting control loop, seed={seed}.\n{NON_DETERMINISM_DISCLAIMER}");
    let mut flags = Vec::new();
    let mut workload_cnt = 0;
    let mut running_workloads = JoinSet::new();

    let workdir_path = if let Some(workdir_path) = swarm_params.workdir.take() {
        if !std::path::Path::new(&workdir_path).exists() {
            anyhow::bail!("The workdir path does not exist");
        }
        PathBuf::from_str(&workdir_path).unwrap()
    } else {
        std::env::temp_dir()
    };

    // TODO: Currently reproducibility is broken, will be fixed in a follow up.
    // In the vision of more complex resource allocation mechanisms
    // and swarm testing, requiring a single seed that can reproduce also the
    // preparation of a workload seems too big of a constraint.
    // One way to enable reproducibility is to store all
    // the workload data needed to just run it.
    let resource_alloc = Arc::new(Mutex::new(ResourceAllocator::new(
        workdir_path.clone(),
        seed,
        swarm_params.max_disk,
        swarm_params.max_memory,
    )?));

    loop {
        // Collect any finished workload.
        while let Some(maybe_flag) = running_workloads.try_join_next() {
            if let Some(flag) = maybe_flag?? {
                print_flag(&flag);
                flags.push(flag);
            }
        }

        loop {
            let workload_id = workload_cnt;
            let workload_seed = seed + workload_cnt;
            let workload_dir = init_workload_dir(workdir_path.clone(), workload_id);

            let Ok(workload) = Workload::new(
                workload_seed,
                workload_dir,
                workload_id,
                resource_alloc.clone(),
            ) else {
                break;
            };

            workload_cnt += 1;

            let _ = running_workloads.spawn(run_workload(
                cancel_token.clone(),
                workload_seed,
                workload_id,
                workload,
            ));
        }

        // The maximum number of workload, based on available resources, has been spawned.
        //
        // Here is safe to wait on a workload completion because if
        // ctrl-c is received, the cancel_token will stop the workload
        // and thus allow all workloads to conclude early.
        //
        // UNWRAP: at least one workload execution was spawned.
        // The execution is expected to properly reach completion.
        let workload_result = running_workloads.join_next().await.unwrap()?;
        // The execution could have returned an error or an optional flag.
        let maybe_flag = workload_result?;
        if let Some(flag) = maybe_flag {
            print_flag(&flag);
            flags.push(flag);
        }

        if cancel_token.is_cancelled() {
            break;
        }
        if flags.len() >= swarm_params.flag_limit {
            info!("Flag limit reached. Exiting.");
            break;
        }
    }

    // Wait for active workloads to be cancelled properly.
    for workload_result in running_workloads.join_all().await {
        if let Some(flag) = workload_result? {
            flags.push(flag);
        }
    }

    for flag in flags {
        print_flag(&flag);
    }
    Ok(())
}

async fn run_single_workload(
    cancel_token: CancellationToken,
    mut run_params: RunParams,
) -> Result<()> {
    let workdir_path = if let Some(workdir_path) = run_params.workdir.take() {
        if !std::path::Path::new(&workdir_path).exists() {
            anyhow::bail!("The workdir path does not exist");
        }
        PathBuf::from(&workdir_path)
    } else {
        std::env::temp_dir()
    };

    let workload_dir = init_workload_dir(workdir_path.clone(), 0 /* workload_id */);

    let workload = Workload::new_with_data(
        run_params.seed,
        workload_dir,
        0, /* workload_id */
        run_params.ensure_snapshot,
        run_params.assigned_disk,
        run_params.assigned_memory,
    );

    let maybe_flag = run_workload(cancel_token.clone(), run_params.seed, 0, workload).await?;
    if let Some(flag) = maybe_flag {
        print_flag(&flag);
    }
    Ok(())
}
