//! Experimental CUDA activity profiles. CPU sampling is independently optional.
//! The optional, SDK-specific native adapter only sends bounded records. Rust
//! owns attribution leases, joins, aggregation, deadlines and HTTP delivery.
use crate::{capture::Token, export::FlushError, profiles::upload};
use crossbeam_channel::{bounded, Receiver, Sender};
use pyroscope::pyroscope::PyroscopeConfig;
use std::{
    cell::Cell,
    collections::HashMap,
    ffi::{CStr, CString},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Condvar, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

#[path = "gpuprofileencode.rs"]
mod encode;

const CAPACITY: usize = 8192;
const CONTEXT_TTL: Duration = Duration::from_secs(30);
const MAX_GROUPS: usize = 8192;
static STATE: OnceLock<State> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static STOPPING: AtomicBool = AtomicBool::new(false);
thread_local! { static IDS: Cell<[u64; 3]> = const { Cell::new([0; 3]) }; }

/// ABI version 1, shared with native/gpu_adapter.h. No SDK types cross the ABI.
#[repr(C)]
#[derive(Clone, Copy)]
struct Event {
    kind: u32,
    correlation: u32,
    start: u64,
    end: u64,
    device: u32,
    stream: u32,
    count: u64,
    name: [u8; 256],
}

#[derive(Clone)]
struct Owner {
    ids: [u64; 3],
    element: Option<(u64, Arc<crate::profilecontext::Lease>)>,
    submitted: u64,
    seen: Instant,
}
struct Message {
    event: Event,
    owner: Option<Owner>,
}
enum Command {
    Flush(Sender<Result<(), FlushError>>),
    Stop,
}
struct State {
    events: Sender<Message>,
    commands: Sender<Command>,
    counters: Arc<Counters>,
    done: (Mutex<Option<Result<(), FlushError>>>, Condvar),
}
#[derive(Default)]
struct Counters {
    kernels: AtomicU64,
    attributed: AtomicU64,
    correlated: AtomicU64,
    unmatched: AtomicU64,
    invalid: AtomicU64,
    dropped_events: AtomicU64,
    dropped_records: AtomicU64,
    context_capacity_rejections: AtomicU64,
    pending_contexts: AtomicU64,
    dropped_processing: AtomicU64,
    delivery: Arc<upload::Counters>,
}

/// GPU counters count kernel activities, not periodic CPU samples. Pending
/// contexts are bounded launch associations awaiting a kernel activity.
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuProfileStats {
    pub kernels: u64,
    pub attributed_kernels: u64,
    pub correlated_kernels: u64,
    pub unmatched_kernels: u64,
    pub invalid_timestamps: u64,
    pub dropped_events: u64,
    pub dropped_records: u64,
    pub context_capacity_rejections: u64,
    pub pending_contexts: u64,
    pub dropped_processing: u64,
    pub reported_kernels: u64,
    pub accepted_kernels: u64,
    pub failed_kernels: u64,
    pub dropped_upload: u64,
    pub pending_kernels: u64,
    pub pending_uploads: u64,
    pub accepted_profiles: u64,
    pub failed_exports: u64,
}
pub fn gpu_profile_stats() -> GpuProfileStats {
    let Some(s) = STATE.get() else {
        return GpuProfileStats::default();
    };
    let c = &s.counters;
    let d = &c.delivery;
    let reported = d.reported_samples.load(Relaxed);
    let accepted = d.accepted_samples.load(Relaxed);
    let failed = d.failed_samples.load(Relaxed);
    GpuProfileStats {
        kernels: c.kernels.load(Relaxed),
        attributed_kernels: c.attributed.load(Relaxed),
        correlated_kernels: c.correlated.load(Relaxed),
        unmatched_kernels: c.unmatched.load(Relaxed),
        invalid_timestamps: c.invalid.load(Relaxed),
        dropped_events: c.dropped_events.load(Relaxed),
        dropped_records: c.dropped_records.load(Relaxed),
        context_capacity_rejections: c.context_capacity_rejections.load(Relaxed),
        pending_contexts: c.pending_contexts.load(Relaxed),
        dropped_processing: c.dropped_processing.load(Relaxed),
        reported_kernels: reported,
        accepted_kernels: accepted,
        failed_kernels: failed,
        dropped_upload: d.dropped_upload.load(Relaxed),
        pending_kernels: reported.saturating_sub(accepted).saturating_sub(failed),
        pending_uploads: d
            .reported_profiles
            .load(Relaxed)
            .saturating_sub(d.accepted_profiles.load(Relaxed))
            .saturating_sub(d.failed_exports.load(Relaxed)),
        accepted_profiles: d.accepted_profiles.load(Relaxed),
        failed_exports: d.failed_exports.load(Relaxed),
    }
}
pub(crate) fn active() -> bool {
    ACTIVE.load(Relaxed)
}
pub(crate) fn publish(token: Option<&Token>) -> bool {
    if !active() {
        return false;
    }
    let ids = token.map_or([0; 3], |token| {
        let trace = token.trace_id.to_bytes();
        [
            u64::from_be_bytes(trace[..8].try_into().unwrap()),
            u64::from_be_bytes(trace[8..].try_into().unwrap()),
            u64::from_be_bytes(token.span_id.to_bytes()),
        ]
    });
    IDS.try_with(|value| value.set(ids)).is_ok() && token.is_some()
}

// Called on the launching host thread for launches, and CUPTI's delivery
// threads for kernels. No GObject calls, registry locks, networking or waits.
unsafe extern "C" fn receive(event: *const Event) {
    let Some(state) = STATE.get() else { return };
    if event.is_null() {
        return;
    }
    // SAFETY: adapter ABI guarantees a valid record for this callback's duration.
    let event = unsafe { *event };
    if event.kind == 3 {
        state
            .counters
            .dropped_records
            .fetch_add(event.count, Relaxed);
        return;
    }
    let owner = (event.kind == 1).then(|| Owner {
        ids: if active() && crate::profilecontext::correlation_allowed() {
            IDS.try_with(Cell::get).unwrap_or([0; 3])
        } else {
            [0; 3]
        },
        element: if active() {
            crate::profilecontext::gpu_snapshot()
        } else {
            None
        },
        submitted: event.start,
        seen: Instant::now(),
    });
    if state.events.try_send(Message { event, owner }).is_err() {
        state.counters.dropped_events.fetch_add(1, Relaxed);
    }
}

struct Adapter {
    flush: unsafe extern "C" fn(u32) -> i32,
}
impl Adapter {
    fn open(path: &str) -> Result<Self, String> {
        let path = CString::new(path).map_err(|_| "GPU adapter path contains NUL")?;
        // SAFETY: loading the user-selected adapter; symbols use the documented
        // v1 C ABI. Keep the DSO resident: CUPTI can retain callback addresses.
        unsafe {
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if handle.is_null() {
                let error = libc::dlerror();
                return Err(if error.is_null() {
                    "cannot load GPU adapter".into()
                } else {
                    CStr::from_ptr(error).to_string_lossy().into_owned()
                });
            }
            let start = libc::dlsym(handle, c"mars_gpu_start_v1".as_ptr());
            let flush = libc::dlsym(handle, c"mars_gpu_flush_v1".as_ptr());
            if start.is_null() || flush.is_null() {
                return Err("GPU adapter ABI v1 symbols missing".into());
            }
            let start: unsafe extern "C" fn(unsafe extern "C" fn(*const Event)) -> i32 =
                std::mem::transmute(start);
            let flush: unsafe extern "C" fn(u32) -> i32 = std::mem::transmute(flush);
            let status = start(receive);
            if status != 0 {
                return Err(format!("CUPTI adapter initialization failed ({status}); check SDK/driver compatibility and profiling access"));
            }
            Ok(Self { flush })
        }
    }
    fn flush(&self, stop: bool) -> Result<(), FlushError> {
        // SAFETY: loaded v1 function; only this worker invokes adapter controls.
        if unsafe { (self.flush)(u32::from(stop)) } == 0 {
            Ok(())
        } else {
            Err(FlushError::ExportFailed)
        }
    }
}

#[derive(Hash, PartialEq, Eq)]
struct Key {
    bucket: u64,
    ids: [u64; 3],
    element: Arc<str>,
    kernel: String,
    device: u32,
}
#[derive(Default)]
struct Value {
    nanos: u64,
    kernels: u64,
}
#[derive(Default)]
struct Joiner {
    owners: HashMap<u32, Owner>,
    groups: HashMap<Key, Value>,
}
impl Joiner {
    fn process(&mut self, message: Message, counters: &Counters) {
        let event = message.event;
        if let Some(owner) = message.owner {
            if self.owners.len() >= CAPACITY && !self.owners.contains_key(&event.correlation) {
                counters.context_capacity_rejections.fetch_add(1, Relaxed);
            } else {
                self.owners.insert(event.correlation, owner);
            }
            return;
        }
        if event.kind != 2 {
            return;
        }
        counters.kernels.fetch_add(1, Relaxed);
        // The supported ordinary/cooperative single-device launch APIs produce
        // one kernel activity per correlation ID. Release its lease immediately:
        // retaining completed launches until the TTL exhausts the bounded table
        // on sustained workloads. Graph replay needs a separate join lifetime.
        let owner = self.owners.remove(&event.correlation);
        if event.start == 0 || event.end <= event.start {
            counters.invalid.fetch_add(1, Relaxed);
            return;
        }
        if owner.is_none() {
            counters.unmatched.fetch_add(1, Relaxed);
        }
        let (ids, submitted, element) = owner.map_or_else(
            || ([0; 3], unix_nanos(), Arc::from("[unattributed GPU]")),
            |owner| {
                if owner.ids != [0; 3] {
                    counters.correlated.fetch_add(1, Relaxed);
                }
                let frame = owner.element.as_ref().map_or_else(
                    || Arc::from("[unattributed GPU]"),
                    |(key, _lease)| {
                        counters.attributed.fetch_add(1, Relaxed);
                        crate::profilecontext::frame(*key)
                    },
                );
                (owner.ids, owner.submitted, frame)
            },
        );
        let len = event
            .name
            .iter()
            .position(|v| *v == 0)
            .unwrap_or(event.name.len());
        let kernel = String::from_utf8_lossy(&event.name[..len]).into_owned();
        let key = Key {
            bucket: submitted / 1_000_000_000,
            ids,
            element,
            kernel,
            device: event.device,
        };
        if self.groups.len() >= MAX_GROUPS && !self.groups.contains_key(&key) {
            counters.dropped_processing.fetch_add(1, Relaxed);
            return;
        }
        let value = self.groups.entry(key).or_default();
        value.nanos = value.nanos.saturating_add(event.end - event.start);
        value.kernels += 1;
    }
    fn expire(&mut self, counters: &Counters) {
        self.owners
            .retain(|_, owner| owner.seen.elapsed() < CONTEXT_TTL);
        counters
            .pending_contexts
            .store(self.owners.len() as u64, Relaxed);
    }
    fn report(
        &mut self,
        uploader: &upload::Uploader,
        counters: &Counters,
        drain: bool,
    ) -> Result<(), FlushError> {
        let mut result = Ok(());
        for (batch, kernels) in encode::batches(std::mem::take(&mut self.groups), counters) {
            // Account every batch even if the upload worker has disconnected.
            result = result.and(uploader.submit(batch, kernels, drain));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(correlation: u32) -> Message {
        Message {
            event: Event {
                kind: 1,
                correlation,
                start: 1,
                end: 2,
                device: 0,
                stream: 0,
                count: 0,
                name: [0; 256],
            },
            owner: Some(Owner {
                ids: [1, 2, 3],
                element: None,
                submitted: 1,
                seen: Instant::now(),
            }),
        }
    }

    fn kernel(correlation: u32) -> Message {
        let mut message = launch(correlation);
        message.event.kind = 2;
        message.owner = None;
        message
    }

    #[test]
    fn sustained_completed_launches_do_not_exhaust_context_capacity() {
        let mut joiner = Joiner::default();
        let counters = Counters::default();
        for id in 0..(CAPACITY * 2) as u32 {
            joiner.process(launch(id), &counters);
            joiner.process(kernel(id), &counters);
        }
        joiner.expire(&counters);
        assert_eq!(counters.correlated.load(Relaxed), (CAPACITY * 2) as u64);
        assert_eq!(counters.unmatched.load(Relaxed), 0);
        assert_eq!(counters.context_capacity_rejections.load(Relaxed), 0);
        assert_eq!(counters.pending_contexts.load(Relaxed), 0);
        assert_eq!(joiner.groups.len(), 1);
    }

    #[test]
    fn in_flight_launches_remain_bounded_and_invalid_completion_releases_slot() {
        let mut joiner = Joiner::default();
        let counters = Counters::default();
        for id in 0..=CAPACITY as u32 {
            joiner.process(launch(id), &counters);
        }
        assert_eq!(joiner.owners.len(), CAPACITY);
        assert_eq!(counters.context_capacity_rejections.load(Relaxed), 1);
        let mut invalid = kernel(0);
        invalid.event.end = 0;
        joiner.process(invalid, &counters);
        assert_eq!(counters.invalid.load(Relaxed), 1);
        joiner.process(launch(CAPACITY as u32 + 1), &counters);
        assert_eq!(joiner.owners.len(), CAPACITY);
        assert_eq!(counters.context_capacity_rejections.load(Relaxed), 1);
    }
}
fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as u64
}

pub(crate) fn initialize(
    resource: &opentelemetry_sdk::Resource,
    owns_retirement: bool,
    settings: &crate::profilesettings::Config,
) -> Result<(), String> {
    let url = settings.url.clone();
    let timeout = settings.timeout_ms;
    let path = settings.gpu_adapter.clone();
    let name = resource
        .iter()
        .find(|(key, _)| key.as_str() == "service.name")
        .map(|(_, value)| value.to_string())
        .unwrap_or_else(|| "mars-otel".into());
    let config = PyroscopeConfig::new(url, name, 1, "mars-cupti", env!("CARGO_PKG_VERSION"));
    let (tx, events) = bounded(CAPACITY);
    let (commands, rx) = bounded(8);
    let counters = Arc::new(Counters::default());
    STATE
        .set(State {
            events: tx,
            commands,
            counters: counters.clone(),
            done: (Mutex::new(None), Condvar::new()),
        })
        .map_err(|_| "GPU profiler already initialized")?;
    let (ready, started) = bounded(1);
    std::thread::Builder::new()
        .name("mars-gpu-profiles".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(|| {
                worker(
                    path,
                    config,
                    Duration::from_millis(timeout),
                    counters,
                    events,
                    rx,
                    ready,
                    owns_retirement,
                )
            })
            .unwrap_or(Err(FlushError::ExportFailed));
            ACTIVE.store(false, Relaxed);
            let state = STATE.get().unwrap();
            *state.done.0.lock().unwrap() = Some(result);
            state.done.1.notify_all();
        })
        .map_err(|e| e.to_string())?;
    started
        .recv()
        .map_err(|_| "GPU worker failed to initialize")?
}

#[allow(clippy::too_many_arguments)]
fn worker(
    path: String,
    config: PyroscopeConfig,
    timeout: Duration,
    counters: Arc<Counters>,
    events: Receiver<Message>,
    commands: Receiver<Command>,
    ready: Sender<Result<(), String>>,
    owns_retirement: bool,
) -> Result<(), FlushError> {
    let create = || upload::Uploader::start(config.clone(), timeout, counters.delivery.clone());
    let initial = create();
    if initial.is_err() {
        let _ = ready.send(Err("GPU uploader initialization failed".into()));
        return Err(FlushError::ExportFailed);
    }
    let mut uploader = Some(initial?);
    let adapter = match Adapter::open(&path) {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Err(FlushError::ExportFailed);
        }
    };
    ACTIVE.store(true, Relaxed);
    let _ = ready.send(Ok(()));
    let mut joiner = Joiner::default();
    let mut next = Instant::now() + Duration::from_secs(1);
    let result = loop {
        let command = crossbeam_channel::select! {
            recv(commands) -> command => Some(command.unwrap_or(Command::Stop)),
            recv(events) -> message => {
                if let Ok(message) = message { joiner.process(message, &counters); }
                if Instant::now() < next { continue; }
                None
            },
            default(next.saturating_duration_since(Instant::now())) => None,
        };
        let stop = matches!(command, Some(Command::Stop));
        let flushed = adapter.flush(stop);
        // A bounded pass prevents continuously arriving events starving cleanup.
        for message in events.try_iter().take(CAPACITY) {
            joiner.process(message, &counters);
        }
        let reported = joiner.report(uploader.as_ref().unwrap(), &counters, command.is_some());
        joiner.expire(&counters);
        if owns_retirement || crate::profiles::cpu_reports_finished() {
            // The CPU worker otherwise owns retirement. GPU leases keep its
            // descriptions alive until all pending references have gone away.
            crate::profilecontext::after_report(crate::profilecontext::before_drain());
        }
        match command {
            Some(Command::Stop) => break flushed.and(reported),
            Some(Command::Flush(reply)) => {
                let delivered = uploader.take().unwrap().finish();
                match create() {
                    Ok(new) => uploader = Some(new),
                    Err(error) => {
                        let _ = reply.send(Err(error.clone()));
                        break Err(error);
                    }
                }
                let _ = reply.send(flushed.and(reported).and(delivered));
            }
            None => {
                if let Err(error) = flushed.and(reported) {
                    break Err(error);
                }
            }
        }
        next = Instant::now() + Duration::from_secs(1);
    };
    // Also quiesce the adapter on exporter/control errors, never from callbacks.
    let stopped = adapter.flush(true);
    joiner.owners.clear();
    counters.pending_contexts.store(0, Relaxed);
    if owns_retirement || crate::profiles::cpu_reports_finished() {
        crate::profilecontext::after_report(crate::profilecontext::before_drain());
    }
    result
        .and(stopped)
        .and(uploader.map_or(Ok(()), |u| u.finish()))
}

pub(crate) fn drain(timeout: Duration, stop: bool) -> Result<(), FlushError> {
    let Some(state) = STATE.get() else {
        return Ok(());
    };
    let start = Instant::now();
    if stop {
        ACTIVE.store(false, Relaxed);
        if !STOPPING.swap(true, Relaxed) {
            std::thread::spawn(move || {
                let _ = state.commands.send(Command::Stop);
            });
        }
        let result = state.done.0.lock().unwrap();
        let (result, _) = state
            .done
            .1
            .wait_timeout_while(result, timeout.saturating_sub(start.elapsed()), |r| {
                r.is_none()
            })
            .unwrap();
        result.clone().unwrap_or(Err(FlushError::Timeout))
    } else {
        if STOPPING.load(Relaxed) {
            return Err(FlushError::Closed);
        }
        let (reply, result) = bounded(1);
        state
            .commands
            .send_timeout(Command::Flush(reply), timeout)
            .map_err(|error| {
                if error.is_timeout() {
                    FlushError::Timeout
                } else {
                    FlushError::Closed
                }
            })?;
        result
            .recv_timeout(timeout.saturating_sub(start.elapsed()))
            .map_err(|error| {
                if error.is_timeout() {
                    FlushError::Timeout
                } else {
                    FlushError::Closed
                }
            })?
    }
}
