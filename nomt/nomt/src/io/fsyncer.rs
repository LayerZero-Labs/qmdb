use parking_lot::{Condvar, Mutex};
use std::{fs::File, sync::Arc};

#[derive(Debug)]
enum State {
    Idle,
    Started,
    Done(Result<(), std::io::Error>),
    HandleDead,
}

impl State {
    fn force_take_done(&mut self) -> Result<(), std::io::Error> {
        let s = std::mem::replace(self, State::Idle);
        if let State::Done(res) = s {
            res
        } else {
            panic!("force_take_done called on non-done state");
        }
    }
}

struct Shared {
    cv: Condvar,
    s: Mutex<State>,
}

/// Fsyncer is a helper that allows to fsync a file in a non-blocking manner.
///
/// It spawns a thread that will fsync the file in the background.
///
/// The expected usage is from two threads: the one that calls [`Self::fsync`] and the one that calls
/// [`Self::wait`].
pub struct Fsyncer {
    shared: Arc<Shared>,
}

impl Fsyncer {
    /// Creates a new fsyncer with the given file descriptor and identifier.
    pub fn new(name: &'static str, fd: Arc<File>) -> Self {
        let name = format!("nomt-fsyncer-{}", name);
        let shared = Arc::new(Shared {
            cv: Condvar::new(),
            s: Mutex::new(State::Idle),
        });
        let _thread = std::thread::Builder::new()
            .name(name)
            .spawn({
                let shared = shared.clone();
                move || {
                    worker(fd, shared);
                }
            })
            .expect("failed to spawn fsyncer thread");
        Fsyncer { shared }
    }

    /// Issues a fsync request.
    ///
    /// # Panics
    ///
    /// Panics if there is an outstanding fsync operation that hasn't been consumed by
    /// [`Self::wait()`] yet.
    ///
    /// Make sure to call [`Self::wait()`] to consume any previous fsync result before issuing a new
    /// request.
    pub fn fsync(&self) {
        let mut s_guard = self.shared.s.lock();
        assert!(matches!(&*s_guard, State::Idle));
        *s_guard = State::Started;
        self.shared.cv.notify_all();
    }

    /// Waits for the fsync to complete and consumes the result.
    ///
    /// This blocks until a synchronization initiated by [`Self::fsync`] completes. If no fsync has been
    /// initiated yet, this will block until one is both started and completed. After consuming the result,
    /// subsequent calls will block until the next `fsync()` operation finishes.
    pub fn wait(&self) -> Result<(), std::io::Error> {
        let mut s_guard = self.shared.s.lock();
        self.shared
            .cv
            .wait_while(&mut s_guard, |s| !matches!(s, State::Done(_)));
        s_guard.force_take_done()
    }
}

impl Drop for Fsyncer {
    fn drop(&mut self) {
        let mut s_guard = self.shared.s.lock();
        *s_guard = State::HandleDead;
        self.shared.cv.notify_all();
    }
}

fn worker(fd: Arc<File>, shared: Arc<Shared>) {
    let bomb = Bomb;
    'outer: loop {
        let mut s_guard = shared.s.lock();
        shared.cv.wait_while(&mut s_guard, |state| {
            !matches!(state, State::Started | State::HandleDead)
        });
        if matches!(&*s_guard, State::HandleDead) {
            break 'outer;
        }
        assert!(matches!(&*s_guard, State::Started | State::Done(_)));
        drop(s_guard);

        let sync_result = fd.sync_all();

        let mut s_guard = shared.s.lock();
        if matches!(&*s_guard, State::HandleDead) {
            break 'outer;
        }
        *s_guard = State::Done(sync_result);
        shared.cv.notify_all();
    }
    bomb.defuse();

    struct Bomb;
    impl Bomb {
        fn defuse(self) {
            std::mem::forget(self);
        }
    }
    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("worker panicked");
        }
    }
}
