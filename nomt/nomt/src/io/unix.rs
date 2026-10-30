use super::{CompleteIo, IoCommand, IoKind, IoKindResult, IoPacket, PagePool, PAGE_SIZE};
use crossbeam_channel::{Receiver, Sender};
use threadpool::ThreadPool;

pub fn check_iou_permissions() -> super::IoUringPermission {
    super::IoUringPermission::NotSupported
}

pub fn start_io_worker(
    page_pool: PagePool,
    io_workers_tp: &ThreadPool,
    io_workers: usize,
) -> Sender<IoPacket> {
    let (command_tx, command_rx) = crossbeam_channel::unbounded();

    for _ in 0..io_workers {
        spawn_worker_thread(page_pool.clone(), io_workers_tp, command_rx.clone());
    }

    command_tx
}

fn spawn_worker_thread(
    page_pool: PagePool,
    io_workers_tp: &ThreadPool,
    command_rx: Receiver<IoPacket>,
) {
    let work = move || loop {
        let Ok(packet) = command_rx.recv() else {
            // Why the `drop` here?
            //
            // `command_rx` receives the IoPacket's which are ultimately parameterized by buffers.
            // Those buffers are allocated in the `page_pool`. If the `page_pool` is deallocated
            // before this worker thread is done, that's a use-after-free.
            //
            // So in other words, we plumb `page_pool` all the way here and drop it here only to
            // ensure safety.
            drop(page_pool);
            return;
        };
        let complete = execute(packet.command);
        let _ = packet.completion_sender.send(complete);
    };

    io_workers_tp.execute(work);
}

fn execute(mut command: IoCommand) -> CompleteIo {
    let result = loop {
        let res = match command.kind {
            IoKind::Read(fd, page_index, ref mut page) => unsafe {
                libc::pread(
                    fd,
                    page.as_mut_ptr() as *mut libc::c_void,
                    PAGE_SIZE as libc::size_t,
                    (page_index * PAGE_SIZE as u64) as libc::off_t,
                )
            },
            IoKind::Write(fd, page_index, ref page) => unsafe {
                libc::pwrite(
                    fd,
                    page.as_ptr() as *const libc::c_void,
                    PAGE_SIZE as libc::size_t,
                    (page_index * PAGE_SIZE as u64) as libc::off_t,
                )
            },
            IoKind::WriteArc(fd, page_index, ref page) => unsafe {
                let page: &[u8] = &*page;
                libc::pwrite(
                    fd,
                    page.as_ptr() as *const libc::c_void,
                    PAGE_SIZE as libc::size_t,
                    (page_index * PAGE_SIZE as u64) as libc::off_t,
                )
            },
            IoKind::WriteRaw(fd, page_index, ref mut page) => unsafe {
                libc::pwrite(
                    fd,
                    page.as_ptr() as *const libc::c_void,
                    PAGE_SIZE as libc::size_t,
                    (page_index * PAGE_SIZE as u64) as libc::off_t,
                )
            },
        };
        match command.kind.get_result(res) {
            IoKindResult::Ok => break Ok(()),
            IoKindResult::Err => break Err(std::io::Error::last_os_error()),
            IoKindResult::Retry => (),
        }
    };

    CompleteIo { command, result }
}
