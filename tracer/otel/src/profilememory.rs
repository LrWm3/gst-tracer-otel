//! One process-wide, post-preroll allocator trim. Never trim in a tracer hook,
//! a sampler signal handler, or while holding a GStreamer/CUPTI lock.
use crate::{export::FlushError, profiles::ProfileStats};
use gstreamer::{self as gst, prelude::*};
use std::{
    sync::{
        atomic::{
            AtomicBool, AtomicU64, AtomicU8, Ordering::AcqRel, Ordering::Acquire, Ordering::Release,
        },
        Condvar, Mutex,
    },
    time::{Duration, Instant},
};

const DISABLED: u8 = 0;
const ARMED: u8 = 1;
const QUEUED: u8 = 2;
const RUNNING: u8 = 3;
const DONE: u8 = 4;
const CANCELLED: u8 = 5;
const FAILED: u8 = 6;
const SUPPORTED: bool = cfg!(all(
    feature = "profiles",
    target_os = "linux",
    target_env = "gnu"
));

#[derive(Default)]
struct Trim {
    phase: AtomicU8,
    released: AtomicBool,
    elapsed_ns: AtomicU64,
    completion: (Mutex<()>, Condvar),
}
static TRIM: Trim = Trim {
    phase: AtomicU8::new(DISABLED),
    released: AtomicBool::new(false),
    elapsed_ns: AtomicU64::new(0),
    completion: (Mutex::new(()), Condvar::new()),
};

impl Trim {
    fn claim(&self) -> bool {
        self.phase
            .compare_exchange(ARMED, QUEUED, Acquire, Acquire)
            .is_ok()
    }

    fn finish(&self, phase: u8) {
        let _guard = self.completion.0.lock().unwrap();
        self.phase.store(phase, Release);
        self.completion.1.notify_all();
    }

    fn run(&self, trim: impl FnOnce() -> bool) {
        if self
            .phase
            .compare_exchange(QUEUED, RUNNING, Acquire, Acquire)
            .is_err()
        {
            return; // Shutdown cancelled queued work before this thread started.
        }
        let start = Instant::now();
        let released = trim();
        self.elapsed_ns.store(
            start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Release,
        );
        self.released.store(released, Release);
        self.finish(DONE);
    }

    fn cancel(&self) {
        let _guard = self.completion.0.lock().unwrap();
        let _ = self
            .phase
            .compare_exchange(ARMED, CANCELLED, Acquire, Acquire);
        let _ = self
            .phase
            .compare_exchange(QUEUED, CANCELLED, Acquire, Acquire);
        self.completion.1.notify_all();
    }

    fn wait(&self, timeout: Duration) -> Result<(), FlushError> {
        let guard = self.completion.0.lock().unwrap();
        let _guard = self
            .completion
            .1
            .wait_timeout_while(guard, timeout, |_| {
                matches!(self.phase.load(Acquire), QUEUED | RUNNING)
            })
            .unwrap();
        if matches!(self.phase.load(Acquire), QUEUED | RUNNING) {
            Err(FlushError::Timeout)
        } else {
            Ok(())
        }
    }
}

/// Called only after all selected profile sources initialize successfully.
pub(crate) fn configure(enabled: bool) {
    if enabled && SUPPORTED {
        // Process-wide profiling cannot restart after shutdown; neither can trim.
        let _ = TRIM
            .phase
            .compare_exchange(DISABLED, ARMED, AcqRel, Acquire);
    }
}

pub(crate) fn enabled() -> bool {
    TRIM.phase.load(Acquire) == ARMED
}

fn ready(element: &gst::Element, message: &gst::MessageRef) -> bool {
    let gst::MessageView::StateChanged(change) = message.view() else {
        return false;
    };
    change.current() == gst::State::Playing
        && change.pending() == gst::State::VoidPending
        && element.is::<gst::Pipeline>()
        && message.src() == Some(element.upcast_ref())
        && element.parent().is_none()
}

/// Observe the pipeline's committed state, including asynchronous transitions.
/// This hook does not consume bus messages or retain pipeline references.
pub(crate) fn message(element: &gst::Element, message: &gst::MessageRef) {
    if !enabled() || !ready(element, message) || !TRIM.claim() {
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("mars-heap-trim".into())
        .spawn(|| {
            TRIM.run(|| {
                #[cfg(all(feature = "profiles", target_os = "linux", target_env = "gnu"))]
                // SAFETY: glibc's thread-safe operation only returns unused allocator
                // pages. No application pointers, signal handlers or plugin locks.
                {
                    unsafe { libc::malloc_trim(0) != 0 }
                }
                #[cfg(not(all(feature = "profiles", target_os = "linux", target_env = "gnu")))]
                {
                    false
                }
            });
            if TRIM.phase.load(Acquire) == DONE {
                gst::debug!(
                    crate::tracer::CAT,
                    "Post-init allocator trim: released={}, elapsed_ns={}",
                    TRIM.released.load(Acquire),
                    TRIM.elapsed_ns.load(Acquire)
                );
            }
        })
    {
        TRIM.finish(FAILED);
        // Memory reclamation is best effort and must not disable profiling.
        gst::warning!(
            crate::tracer::CAT,
            "Cannot start allocator trim worker: {error}"
        );
    }
}

pub(crate) fn cancel() {
    TRIM.cancel();
}
pub(crate) fn wait(timeout: Duration) -> Result<(), FlushError> {
    TRIM.wait(timeout)
}
pub(crate) fn snapshot(mut stats: ProfileStats) -> ProfileStats {
    let phase = TRIM.phase.load(Acquire);
    stats.memory_trim_attempts = u64::from(matches!(phase, RUNNING | DONE));
    stats.memory_trim_released = u64::from(TRIM.released.load(Acquire));
    stats.memory_trim_elapsed_ns = TRIM.elapsed_ns.load(Acquire);
    stats.memory_trim_pending = matches!(phase, QUEUED | RUNNING);
    stats.memory_trim_failures = u64::from(phase == FAILED);
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_starts_are_once_only_and_cancellation_prevents_work() {
        let trim = Arc::new(Trim {
            phase: AtomicU8::new(ARMED),
            ..Default::default()
        });
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let trim = trim.clone();
                std::thread::spawn(move || trim.claim())
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
        trim.cancel();
        trim.run(|| panic!("queued work must not run after shutdown"));
        assert_eq!(trim.wait(Duration::ZERO), Ok(()));
        assert!(!trim.claim());
    }

    #[test]
    fn shutdown_deadline_does_not_wait_unboundedly_for_the_allocator() {
        let trim = Arc::new(Trim {
            phase: AtomicU8::new(QUEUED),
            ..Default::default()
        });
        let started = Arc::new(Barrier::new(2));
        let finished = Arc::new(Barrier::new(2));
        let thread = {
            let (trim, started, finished) = (trim.clone(), started.clone(), finished.clone());
            std::thread::spawn(move || {
                trim.run(|| {
                    started.wait();
                    finished.wait();
                    true
                })
            })
        };
        started.wait();
        trim.cancel();
        assert_eq!(trim.wait(Duration::ZERO), Err(FlushError::Timeout));
        finished.wait();
        thread.join().unwrap();
        assert_eq!(trim.wait(Duration::ZERO), Ok(()));
        assert!(trim.released.load(Acquire));
        assert!(!trim.claim());
    }

    #[test]
    fn only_the_top_level_pipeline_committed_playing_message_is_ready() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let child = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add(&child).unwrap();
        let changed = |src: &gst::Element, current, pending| {
            gst::message::StateChanged::builder(gst::State::Paused, current, pending)
                .src(src)
                .build()
        };
        assert!(!ready(
            pipeline.upcast_ref(),
            &changed(&child, gst::State::Playing, gst::State::VoidPending)
        ));
        assert!(!ready(
            pipeline.upcast_ref(),
            &changed(
                pipeline.upcast_ref(),
                gst::State::Paused,
                gst::State::Playing
            )
        ));
        assert!(!ready(
            pipeline.upcast_ref(),
            &changed(
                pipeline.upcast_ref(),
                gst::State::Playing,
                gst::State::Paused
            )
        ));
        assert!(ready(
            pipeline.upcast_ref(),
            &changed(
                pipeline.upcast_ref(),
                gst::State::Playing,
                gst::State::VoidPending
            )
        ));
        let outer = gst::Pipeline::new();
        outer.add(&pipeline).unwrap();
        assert!(!ready(
            pipeline.upcast_ref(),
            &changed(
                pipeline.upcast_ref(),
                gst::State::Playing,
                gst::State::VoidPending
            )
        ));
    }
}
