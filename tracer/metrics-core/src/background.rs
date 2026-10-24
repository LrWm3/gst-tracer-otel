//! Interruptible maintenance shared by the collector's background tasks.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Condvar, Mutex,
    },
    time::{Duration, Instant},
};

static STOPPED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static LOCK: Mutex<()> = Mutex::new(());
static WAKE: Condvar = Condvar::new();

pub fn stopped() -> bool {
    STOPPED.load(Ordering::Acquire)
}

pub fn wait(duration: Duration) -> bool {
    let guard = LOCK.lock().unwrap();
    let _guard = WAKE
        .wait_timeout_while(guard, duration, |_| !stopped())
        .unwrap();
    stopped()
}

pub(crate) fn spawn(run: impl FnOnce() + Send + 'static) {
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            let _guard = LOCK.lock().unwrap();
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            WAKE.notify_all();
        }
    }
    ACTIVE.fetch_add(1, Ordering::SeqCst);
    let done = Done;
    // Failed spawn drops the closure and its count guard as well.
    std::thread::Builder::new()
        .name("mars-metrics-bg".into())
        .spawn(move || {
            let _done = done;
            run();
        })
        .expect("start metrics maintenance");
}

pub fn request_stop() {
    let _guard = LOCK.lock().unwrap();
    STOPPED.store(true, Ordering::Release);
    WAKE.notify_all();
}

pub fn shutdown(timeout: Duration) -> bool {
    let start = Instant::now();
    request_stop();
    let guard = LOCK.lock().unwrap();
    let _guard = WAKE
        .wait_timeout_while(guard, timeout.saturating_sub(start.elapsed()), |_| {
            ACTIVE.load(Ordering::SeqCst) != 0
        })
        .unwrap();
    ACTIVE.load(Ordering::SeqCst) == 0
}
