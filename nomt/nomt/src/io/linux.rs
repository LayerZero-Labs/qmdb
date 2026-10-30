use super::{CompleteIo, IoCommand, IoKind, IoKindResult, IoPacket, PagePool, PAGE_SIZE};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use io_uring::{cqueue, opcode, squeue, types, IoUring};
use slab::Slab;
use std::collections::VecDeque;
use threadpool::ThreadPool;

const RING_CAPACITY: u32 = 1024;

// max number of inflight requests is bounded by the slab.
const MAX_IN_FLIGHT: usize = RING_CAPACITY as usize;

pub fn check_iou_permissions() -> super::IoUringPermission {
    let maybe_ring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
        .setup_single_issuer()
        .build(RING_CAPACITY);

    match maybe_ring {
        // EPERM -> PermissionDenied
        // https://doc.rust-lang.org/stable/src/std/sys/pal/unix/mod.rs.html#278
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            super::IoUringPermission::Denied
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
            println!("
                The device has permission to use io_uring, but does not support the flags required
                for io_uring_setup.  NOMT uses the IORING_SETUP_SINGLE_ISSUER flag for io_uring which
                was introduced in linux 6.0. Please update your kernel to at least version 6.0
                to use NOMT with io_uring.
            ");
            // Note that support for IORING_SETUP_SINGLE_ISSUER was added in linux 6.0.  A 5.x kernel will throw EINVAL on .build()
            super::IoUringPermission::NotSupported
        }
        _ => super::IoUringPermission::Allowed,
    }
}

struct PendingIo {
    command: IoCommand,
    completion_sender: Sender<CompleteIo>,
}

pub fn start_io_worker(
    page_pool: PagePool,
    io_workers_tp: &ThreadPool,
    io_workers: usize,
) -> Sender<IoPacket> {
    // main bound is from the pending slab.
    let (command_tx, command_rx) = crossbeam_channel::unbounded();

    start_workers(page_pool, io_workers_tp, command_rx, io_workers);

    command_tx
}

fn start_workers(
    page_pool: PagePool,
    io_workers_tp: &ThreadPool,
    command_rx: Receiver<IoPacket>,
    io_workers: usize,
) {
    for _ in 0..io_workers {
        io_workers_tp.execute({
            let page_pool = page_pool.clone();
            let command_rx = command_rx.clone();
            move || run_worker(page_pool, command_rx)
        });
    }
}

fn run_worker(page_pool: PagePool, command_rx: Receiver<IoPacket>) {
    let mut pending: Slab<PendingIo> = Slab::with_capacity(MAX_IN_FLIGHT);

    let mut ring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
        .setup_single_issuer()
        .build(RING_CAPACITY)
        .expect("Error building io_uring");

    let (submitter, mut submit_queue, mut complete_queue) = ring.split();
    let mut retries = VecDeque::<IoPacket>::new();

    // Indicates whether the worker detected that it should shutdown.
    let mut shutdown = false;

    loop {
        // 1. process completions.
        if !pending.is_empty() {
            complete_queue.sync();
            while let Some(completion_event) = complete_queue.next() {
                if pending.get(completion_event.user_data() as usize).is_none() {
                    continue;
                }
                let PendingIo {
                    command,
                    completion_sender,
                } = pending.remove(completion_event.user_data() as usize);

                // io_uring never uses errno to pass back error information.
                // Instead, completion_event.result() will contain what the equivalent
                // system call would have returned in case of success,
                // and in case of error completion_event.result() will contain -errno
                let io_uring_res = completion_event.result();
                let syscall_result = if io_uring_res >= 0 { io_uring_res } else { -1 };

                let result = match command.kind.get_result(syscall_result as isize) {
                    IoKindResult::Ok => Ok(()),
                    IoKindResult::Err => Err(std::io::Error::from_raw_os_error(io_uring_res.abs())),
                    IoKindResult::Retry => {
                        retries.push_back(IoPacket {
                            command,
                            completion_sender,
                        });
                        continue;
                    }
                };

                let complete = CompleteIo { command, result };
                let _ = completion_sender.send(complete);
            }
        } else if shutdown {
            // No pending IOs and we are shutting down. That means we can exit the worker.
            //
            // Why the `drop` here? Well, recall that the iou accepts commands parametrized with
            // buffers. These buffers are allocated in the page pool. If the page pool is dropped
            // before the ring is dropped, then that's a use-after-free.
            //
            // So in other words, we plumb `page_pool` all the way here and drop it here only to
            // ensure safety.
            drop(page_pool);
            return;
        }

        // 2. accept new I/O requests when slab has space & submission queue is not full.
        let mut to_submit = false;

        submit_queue.sync();
        while pending.len() < MAX_IN_FLIGHT && !submit_queue.is_full() {
            let next_io = if !retries.is_empty() {
                // re-apply partially failed reads and writes
                // unwrap: known not empty
                retries.pop_front().unwrap()
            } else if pending.is_empty() {
                // block on new I/O if nothing in-flight.
                match command_rx.recv() {
                    Ok(command) => command,
                    Err(_) => {
                        shutdown = true;
                        break;
                    }
                }
            } else {
                match command_rx.try_recv() {
                    Ok(command) => command,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        shutdown = true;
                        break;
                    }
                }
            };

            to_submit = true;
            let pending_index = pending.insert(PendingIo {
                command: next_io.command,
                completion_sender: next_io.completion_sender,
            });

            let entry = submission_entry(&mut pending.get_mut(pending_index).unwrap().command)
                .user_data(pending_index as u64);

            // unwrap: known not full
            unsafe { submit_queue.push(&entry).unwrap() };
        }

        // 3. submit all together.
        if to_submit {
            submit_queue.sync();
        }

        let wait = if pending.len() == MAX_IN_FLIGHT { 1 } else { 0 };

        // Do submit handling EINTR.
        loop {
            match submitter.submit_and_wait(wait) {
                Ok(_submitted) => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                other @ Err(_) => panic!("unexpected error: {:?}", other),
            }
        }
    }
}

fn submission_entry(command: &mut IoCommand) -> squeue::Entry {
    match command.kind {
        IoKind::Read(fd, page_index, ref mut page) => {
            opcode::Read::new(types::Fd(fd), page.as_mut_ptr(), PAGE_SIZE as u32)
                .offset(page_index * PAGE_SIZE as u64)
                .build()
        }
        IoKind::Write(fd, page_index, ref page) => {
            opcode::Write::new(types::Fd(fd), page.as_ptr(), PAGE_SIZE as u32)
                .offset(page_index * PAGE_SIZE as u64)
                .build()
        }
        IoKind::WriteArc(fd, page_index, ref page) => {
            let page: &[u8] = &*page;
            opcode::Write::new(types::Fd(fd), page.as_ptr(), PAGE_SIZE as u32)
                .offset(page_index * PAGE_SIZE as u64)
                .build()
        }
        IoKind::WriteRaw(fd, page_index, ref page) => {
            opcode::Write::new(types::Fd(fd), page.as_ptr(), PAGE_SIZE as u32)
                .offset(page_index * PAGE_SIZE as u64)
                .build()
        }
    }
}
