pub type TaskResult<R> = std::thread::Result<R>;

/// Spawn the given task within the given ThreadPool.
/// Use the provided Sender to send the result of the task execution.
///
/// The result will contain the effective result or the payload
/// of the panic that occurred.
pub fn spawn_task<F, R>(
    thread_pool: &threadpool::ThreadPool,
    task: F,
    tx: crossbeam_channel::Sender<TaskResult<R>>,
) where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    thread_pool.execute(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task()));
        let _ = tx.send(res);
    });
}

/// Blocks waiting for completion of the task spawned with [`spawn_task`].
/// It requires the receiver associated to the sender used to spawn the task.
///
/// Panics if the sender is dropped.
pub fn join_task<R>(receiver: &crossbeam_channel::Receiver<TaskResult<R>>) -> R
where
    R: Send + 'static,
{
    // UNWRAP: The sender is not expected to be dropped by the spawned task.
    let res = receiver.recv().unwrap();
    match res {
        Ok(res) => res,
        Err(err_payload) => std::panic::resume_unwind(err_payload),
    }
}
