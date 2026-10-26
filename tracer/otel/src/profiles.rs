//! Experimental embedded CPU sampling and the Pyroscope delivery adapter.
//!
//! Review boundary: all signal handling, context publication, sample ownership,
//! aggregation and profiler lifecycle live here. Capture only publishes fixed
//! IDs. This module deliberately does NOT call Pyroscope's thread-tag API.
//!
//! Linux x86-64 and AArch64. Each participating thread owns a CPU timer and an SPSC
//! sample ring. The signal handler copies registers and a bounded stack window
//! using process_vm_readv, walks frame pointers in that COPY, and snapshots IDs.
//! It never borrows Rust TLS, allocates, locks, symbolizes or calls Pyroscope.
//! A report worker drains rings, symbolizes files with a bounded cache, and
//! supplies compact pprof to the bounded uploader in profileupload.rs.
//!
//! Slots are retained for process lifetime because deleted timers can leave
//! pending signals. GStreamer makes loaded plugin modules resident. The reserved
//! realtime signal remains installed after shutdown; it becomes a no-op. This
//! prototype does not support fork after initialization or unloading an rlib's
//! hosting DSO. Normal GStreamer plugin loading is supported.

use std::time::Duration;

use crate::{capture::Token, export::FlushError};

pub(crate) fn setting(suffix: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("MARS_PROFILE_{suffix}"))
        .or_else(|| std::env::var_os(format!("GST_OTEL_PROFILE_{suffix}")))
}

/// Sampling and delivery counters. `reported_samples` means queued for upload;
/// `accepted_samples` counts samples in acknowledged Pyroscope HTTP requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileStats {
    pub samples: u64,
    pub correlated_samples: u64,
    pub element_samples: u64,
    pub task_samples: u64,
    pub context_capacity_rejections: u64,
    pub context_descriptions: u64,
    pub context_retired: u64,
    pub dropped_full: u64,
    pub timer_overruns: u64,
    pub failed_stack_reads: u64,
    pub truncated_stacks: u64,
    pub registration_failures: u64,
    pub reported_samples: u64,
    pub dropped_processing: u64,
    pub dropped_upload: u64,
    pub active_threads: u64,
    pub accepted_samples: u64,
    pub failed_samples: u64,
    pub accepted_profiles: u64,
    pub failed_exports: u64,
    pub pending_samples: u64,
    pub pending_profiles: u64,
}

#[cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use linux as platform;

#[cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "profilearch.rs"]
mod arch;

#[cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "profilesymbols.rs"]
mod symbols;

#[cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(
        feature = "gpu-profiles",
        target_arch = "x86_64",
        target_arch = "aarch64"
    )
))]
#[path = "profileupload.rs"]
pub(crate) mod upload;

#[cfg(not(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    use super::*;
    pub fn initialize(_: &opentelemetry_sdk::Resource, required: bool) -> Result<(), String> {
        if required || setting("URL").is_some() {
            Err("requires the profiles Cargo feature on Linux x86-64 or AArch64".into())
        } else {
            Ok(())
        }
    }
    pub fn publish(_: Option<&Token>) -> bool {
        false
    }
    pub fn clear() {}
    #[cfg(feature = "profiles")]
    pub fn register_thread() {}
    #[cfg(feature = "profiles")]
    pub fn active() -> bool {
        false
    }
    #[cfg(feature = "profiles")]
    pub fn publish_element(_: u64, _: bool) {}
    pub fn shutdown(_: Duration) -> Result<(), FlushError> {
        Ok(())
    }
    pub fn flush(_: Duration) -> Result<(), FlushError> {
        Ok(())
    }
    pub fn stats() -> ProfileStats {
        ProfileStats::default()
    }
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    pub fn reports_finished() -> bool {
        true
    }
}

#[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
pub(crate) use platform::reports_finished as cpu_reports_finished;
#[cfg(feature = "profiles")]
pub(crate) use platform::{publish_element, register_thread};
pub(crate) fn initialize(
    resource: &opentelemetry_sdk::Resource,
    required: bool,
    sources: crate::ProfileSources,
) -> Result<(), String> {
    if !required && setting("URL").is_none() {
        return Ok(());
    }
    #[cfg(all(feature = "profiles", target_os = "linux"))]
    {
        crate::profilecontext::configure()?;
        crate::nvtxcontext::initialize()?;
    }
    if sources.gpu() {
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        {
            crate::profilecontext::configure()?;
            crate::gpuprofiles::initialize(resource, !sources.cpu())?;
        }
        #[cfg(not(all(feature = "gpu-profiles", target_os = "linux")))]
        return Err("GPU profiles require the gpu-profiles Cargo feature on Linux".into());
    }
    if sources.cpu() {
        let result = platform::initialize(resource, required);
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        if result.is_err() && sources.gpu() {
            let _ = crate::gpuprofiles::drain(Duration::from_secs(1), true);
        }
        result?;
    }
    Ok(())
}
#[cfg(feature = "profiles")]
pub(crate) fn active() -> bool {
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    if crate::gpuprofiles::active() {
        return true;
    }
    platform::active()
}
pub(crate) fn publish(token: Option<&Token>) -> bool {
    let cpu = platform::publish(token);
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    {
        crate::gpuprofiles::publish(token) || cpu
    }
    #[cfg(not(all(feature = "gpu-profiles", target_os = "linux")))]
    cpu
}
pub(crate) fn clear() {
    platform::clear();
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    crate::gpuprofiles::publish(None);
}
pub(crate) fn flush(timeout: Duration) -> Result<(), FlushError> {
    drain(timeout, false)
}
pub(crate) fn shutdown(timeout: Duration) -> Result<(), FlushError> {
    drain(timeout, true)
}
fn drain(timeout: Duration, shutdown: bool) -> Result<(), FlushError> {
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    let start = std::time::Instant::now();
    let cpu = if shutdown {
        platform::shutdown(timeout)
    } else {
        platform::flush(timeout)
    };
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    {
        cpu.and(crate::gpuprofiles::drain(
            timeout.saturating_sub(start.elapsed()),
            shutdown,
        ))
    }
    #[cfg(not(all(feature = "gpu-profiles", target_os = "linux")))]
    cpu
}

pub fn profile_stats() -> ProfileStats {
    let stats = platform::stats();
    #[cfg(feature = "profiles")]
    let stats = ProfileStats {
        context_descriptions: crate::profilecontext::retained(),
        context_retired: crate::profilecontext::retired(),
        context_capacity_rejections: crate::profilecontext::rejected(),
        ..stats
    };
    stats
}

#[cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux {
    use super::symbols::{Symbolization, Symbols};
    use super::*;
    use prost::Message;
    use pyroscope::{
        backend::{Backend, ReportBatch, ReportData, StackFrame, ThreadTag},
        encode::gen::google::{
            Function, Label, Line, Location, Profile, Sample as PprofSample, ValueType,
        },
        error::PyroscopeError,
        pyroscope::PyroscopeConfig,
    };
    use std::{
        cell::{RefCell, UnsafeCell},
        collections::HashMap,
        ffi::c_void,
        sync::{
            atomic::{
                AtomicBool, AtomicI32, AtomicU64, AtomicUsize,
                Ordering::{Acquire, Relaxed, Release, SeqCst},
            },
            Arc, Condvar, Mutex, OnceLock,
        },
    };

    const MAX_THREADS: usize = 64;
    const CAPACITY: usize = 256;
    const MAX_FRAMES: usize = 64;
    const STACK_BYTES: usize = 8192;
    const NO_TIMER: usize = usize::MAX;
    static STATE: OnceLock<Arc<Sampler>> = OnceLock::new();
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    #[cfg(feature = "gpu-profiles")]
    static REPORTS_FINISHED: AtomicBool = AtomicBool::new(false);
    static WORKER: Mutex<Option<Worker>> = Mutex::new(None);
    static STOPPING: AtomicBool = AtomicBool::new(false);
    static STOPPED: (Mutex<Option<Result<(), FlushError>>>, Condvar) =
        (Mutex::new(None), Condvar::new());

    // These are ordinary TLS and are accessed only from normal execution. The
    // handler gets its slot from siginfo and never invokes a TLS initializer.
    thread_local! {
        static REGISTRATION: RefCell<Option<Registration>> = const { RefCell::new(None) };
        static ATTEMPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
    struct Sample {
        ids: [u64; 3], // trace high, trace low, span; zero means uncorrelated
        element: u64,  // immutable description ID plus execution/task discriminator
        tid: i32,
        len: usize,
        pcs: [usize; MAX_FRAMES],
    }
    impl Default for Sample {
        fn default() -> Self {
            Self {
                ids: [0; 3],
                element: 0,
                tid: 0,
                len: 0,
                pcs: [0; MAX_FRAMES],
            }
        }
    }

    struct Slot {
        tid: AtomicI32,
        timer: AtomicUsize,
        low: AtomicUsize,
        high: AtomicUsize,
        valid: AtomicBool,
        ids: [AtomicU64; 3],
        element: AtomicU64,
        correlate_element: AtomicBool,
        head: AtomicUsize,
        tail: AtomicUsize,
        buffers: OnceLock<Box<Buffers>>,
    }
    struct Buffers {
        ring: Box<[UnsafeCell<Sample>]>,
        scratch: UnsafeCell<[u8; STACK_BYTES]>,
    }
    // SAFETY: the same SPSC ownership rules as Slot apply to its buffers.
    unsafe impl Sync for Buffers {}
    // SAFETY: one target-thread signal handler writes scratch and produces ring
    // entries. This signal is masked during its handler (no SA_NODEFER). Only
    // The sole report worker consumes the ring.
    // Release/acquire head/tail transfers exclusive ownership of each entry.
    // Thread retirement never resets indices or modifies a queued sample.
    unsafe impl Sync for Slot {}
    impl Slot {
        fn new() -> Self {
            Self {
                tid: AtomicI32::new(0),
                timer: AtomicUsize::new(NO_TIMER),
                low: AtomicUsize::new(0),
                high: AtomicUsize::new(0),
                valid: AtomicBool::new(false),
                ids: std::array::from_fn(|_| AtomicU64::new(0)),
                element: AtomicU64::new(0),
                correlate_element: AtomicBool::new(true),
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
                buffers: OnceLock::new(),
            }
        }
        // Normal execution only, before arming the timer. Buffers are retained
        // on retirement, so neither pending signals nor the consumer see frees.
        fn prepare(&self) {
            self.buffers.get_or_init(|| {
                Box::new(Buffers {
                    ring: (0..CAPACITY)
                        .map(|_| UnsafeCell::new(Sample::default()))
                        .collect(),
                    scratch: UnsafeCell::new([0; STACK_BYTES]),
                })
            });
        }
        fn publish(&self, ids: Option<[u64; 3]>) {
            self.valid.store(false, Release);
            std::sync::atomic::compiler_fence(SeqCst);
            if let Some(ids) = ids {
                for (field, value) in self.ids.iter().zip(ids) {
                    field.store(value, Relaxed);
                }
                self.valid.store(true, Release);
            }
        }
        fn context(&self) -> [u64; 3] {
            if self.valid.load(Acquire) {
                self.ids.each_ref().map(|v| v.load(Relaxed))
            } else {
                [0; 3]
            }
        }
        // Called only by the sole signal producer (or sequential test harness).
        fn push(&self, sample: Sample) -> bool {
            let Some(buffers) = self.buffers.get() else {
                return false;
            };
            let head = self.head.load(Relaxed);
            if head.wrapping_sub(self.tail.load(Acquire)) == CAPACITY {
                return false;
            }
            // SAFETY: tail publication guarantees this slot is no longer read.
            unsafe {
                *buffers.ring[head % CAPACITY].get() = sample;
            }
            self.head.store(head.wrapping_add(1), Release);
            true
        }
        fn drain(&self, f: &mut impl FnMut(Sample)) {
            let Some(buffers) = self.buffers.get() else {
                return;
            };
            // Fixed snapshot keeps report generation bounded under continuous load.
            let until = self.head.load(Acquire);
            let mut tail = self.tail.load(Relaxed);
            while tail != until {
                // SAFETY: head publication finished the write; the producer
                // cannot reuse this entry before we advance tail below.
                let sample = unsafe { *buffers.ring[tail % CAPACITY].get() };
                f(sample);
                tail = tail.wrapping_add(1);
                self.tail.store(tail, Release);
            }
        }
        fn delete_timer(&self) {
            let timer = self
                .timer
                .swap(NO_TIMER, std::sync::atomic::Ordering::AcqRel);
            if timer != NO_TIMER {
                // SAFETY: swap gives this caller unique deletion ownership.
                unsafe {
                    libc::timer_delete(timer as libc::timer_t);
                }
            }
        }
    }

    #[derive(Default)]
    struct Counters {
        samples: AtomicU64,
        correlated_samples: AtomicU64,
        element_samples: AtomicU64,
        task_samples: AtomicU64,
        dropped_full: AtomicU64,
        timer_overruns: AtomicU64,
        failed_stack_reads: AtomicU64,
        truncated_stacks: AtomicU64,
        registration_failures: AtomicU64,
        dropped_processing: AtomicU64,
    }
    struct Sampler {
        slots: Box<[Slot]>,
        registration: Mutex<()>,
        counters: Counters,
        delivery: Arc<super::upload::Counters>,
        signal: i32,
        rate: u32,
        pid: i32,
        handlers: AtomicUsize,
        symbolization: Symbolization,
    }
    impl Sampler {
        fn new(rate: u32, signal: i32) -> Self {
            Self {
                slots: (0..MAX_THREADS).map(|_| Slot::new()).collect(),
                registration: Mutex::new(()),
                counters: Counters::default(),
                delivery: Arc::new(super::upload::Counters::default()),
                rate,
                signal,
                // SAFETY: getpid has no preconditions.
                pid: unsafe { libc::getpid() },
                handlers: AtomicUsize::new(0),
                symbolization: Symbolization::Full,
            }
        }
        fn stop(&self) {
            let _guard = self.registration.lock().unwrap();
            ACTIVE.store(false, SeqCst);
            for slot in &self.slots {
                slot.delete_timer();
            }
            // Any handler that observed ACTIVE=true registered before that
            // observation. Wait for its final ring write before the last report.
            while self.handlers.load(SeqCst) != 0 {
                std::thread::yield_now();
            }
        }
        fn drain(&self) -> HashMap<Sample, usize> {
            let mut samples = HashMap::new();
            for slot in &self.slots {
                slot.drain(&mut |sample| {
                    *samples.entry(sample).or_insert(0usize) += 1;
                });
            }
            samples
        }
    }

    const MAX_PROFILE_BYTES: usize = 8 * 1024 * 1024;

    // Intern once per report. The upload queue holds protobuf bytes, not a copy
    // of every resolved frame/string for every distinct span/stack combination.
    struct Pprof {
        profile: Profile,
        strings: HashMap<String, i64>,
        functions: HashMap<(i64, i64), u64>,
        locations: HashMap<usize, u64>,
        elements: HashMap<u64, u64>,
    }
    impl Pprof {
        fn new(rate: u32, start: i64, duration: i64) -> Self {
            let mut result = Self {
                profile: Profile {
                    time_nanos: start,
                    duration_nanos: duration.max(1),
                    period: 1_000_000_000 / rate as i64,
                    ..Default::default()
                },
                strings: HashMap::new(),
                functions: HashMap::new(),
                locations: HashMap::new(),
                elements: HashMap::new(),
            };
            result.string("");
            let cpu = result.string("cpu");
            let ns = result.string("nanoseconds");
            let kind = ValueType {
                r#type: cpu,
                unit: ns,
            };
            result.profile.sample_type.push(kind);
            result.profile.period_type = Some(kind);
            result
        }
        fn string(&mut self, value: &str) -> i64 {
            if let Some(id) = self.strings.get(value) {
                return *id;
            }
            let id = self.profile.string_table.len() as i64;
            self.profile.string_table.push(value.into());
            self.strings.insert(value.into(), id);
            id
        }
        fn location(&mut self, pc: usize, frames: &[StackFrame]) {
            let mut lines = Vec::with_capacity(frames.len());
            for frame in frames {
                let name = self.string(frame.name.as_deref().unwrap_or(""));
                let filename = self.string(frame.filename.as_deref().unwrap_or(""));
                let function_id = *self.functions.entry((name, filename)).or_insert_with(|| {
                    let id = self.profile.function.len() as u64 + 1;
                    self.profile.function.push(Function {
                        id,
                        name,
                        filename,
                        ..Default::default()
                    });
                    id
                });
                lines.push(Line {
                    function_id,
                    line: frame.line.unwrap_or(0) as i64,
                });
            }
            let id = self.profile.location.len() as u64 + 1;
            self.profile.location.push(Location {
                id,
                address: pc as u64,
                line: lines,
                ..Default::default()
            });
            self.locations.insert(pc, id);
        }
        fn sample(&mut self, sample: Sample, count: usize) {
            let mut labels = vec![self.label("thread_id", &sample.tid.to_string())];
            if sample.ids != [0; 3] {
                labels.push(self.label(
                    "trace_id",
                    &format!("{:016x}{:016x}", sample.ids[0], sample.ids[1]),
                ));
                labels.push(self.label("span_id", &format!("{:016x}", sample.ids[2])));
            }
            let mut location_id: Vec<_> = sample.pcs[..sample.len]
                .iter()
                .map(|pc| self.locations[pc])
                .collect();
            if crate::profilecontext::enabled() {
                let location = if let Some(id) = self.elements.get(&sample.element) {
                    *id
                } else {
                    let name = self.string(&crate::profilecontext::frame(sample.element));
                    let function_id = self.profile.function.len() as u64 + 1;
                    self.profile.function.push(Function {
                        id: function_id,
                        name,
                        ..Default::default()
                    });
                    let id = self.profile.location.len() as u64 + 1;
                    self.profile.location.push(Location {
                        id,
                        line: vec![Line {
                            function_id,
                            ..Default::default()
                        }],
                        ..Default::default()
                    });
                    self.elements.insert(sample.element, id);
                    id
                };
                // pprof locations are leaf first: append one grouping root.
                // Never duplicate samples into enclosing elements or spans.
                location_id.push(location);
            }
            self.profile.sample.push(PprofSample {
                location_id,
                value: vec![count as i64 * self.profile.period],
                label: labels,
            });
        }
        fn label(&mut self, key: &str, value: &str) -> Label {
            Label {
                key: self.string(key),
                str: self.string(value),
                ..Default::default()
            }
        }
    }

    struct ProfileBackend {
        sampler: Arc<Sampler>,
        symbols: Symbols,
        previous: std::time::Instant,
        pending: u64,
    }
    impl ProfileBackend {
        fn new(sampler: Arc<Sampler>) -> Self {
            let symbols = Symbols::new(sampler.symbolization);
            Self {
                sampler,
                symbols,
                previous: std::time::Instant::now(),
                pending: 0,
            }
        }
    }
    impl Backend for ProfileBackend {
        fn initialize(&mut self) -> pyroscope::Result<()> {
            Ok(())
        }
        fn shutdown(self: Box<Self>) -> pyroscope::Result<()> {
            self.sampler.stop();
            Ok(())
        }
        fn report(&mut self) -> pyroscope::Result<ReportBatch> {
            let _work = gst_mars_metrics_core::monitoring::work(
                gst_mars_metrics_core::monitoring::Stage::Profiles,
                0,
                0,
            );
            let retired = crate::profilecontext::before_drain();
            let samples = self.sampler.drain();
            self.pending = samples.values().sum::<usize>() as u64;
            let mut pcs: Vec<_> = samples
                .keys()
                .flat_map(|s| s.pcs[..s.len].iter().copied())
                .collect();
            pcs.sort_unstable();
            pcs.dedup();
            if !pcs.is_empty() {
                self.symbols.refresh(&pcs);
            }
            let now = std::time::Instant::now();
            let duration = now
                .duration_since(self.previous)
                .as_nanos()
                .min(i64::MAX as u128) as i64;
            self.previous = now;
            let end = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(i64::MAX as u128) as i64;
            let mut builder = Pprof::new(self.sampler.rate, end.saturating_sub(duration), duration);
            for pc in pcs {
                let fallback = [StackFrame {
                    name: Some(format!("0x{pc:x}")),
                    ..Default::default()
                }];
                builder.location(
                    pc,
                    self.symbols
                        .frames
                        .get(&pc)
                        .map(Vec::as_slice)
                        .unwrap_or(&fallback),
                );
            }
            for (sample, count) in samples {
                builder.sample(sample, count);
            }
            let data = if builder.profile.encoded_len() <= MAX_PROFILE_BYTES {
                builder.profile.encode_to_vec()
            } else {
                self.sampler
                    .counters
                    .dropped_processing
                    .fetch_add(self.pending, Relaxed);
                self.pending = 0;
                Pprof::new(self.sampler.rate, end.saturating_sub(duration), duration)
                    .profile
                    .encode_to_vec()
            };
            crate::profilecontext::after_report(retired);
            Ok(ReportBatch {
                // Processing bytes are distinct from server acknowledgement.
                profile_type: "process_cpu".into(),
                data: {
                    gst_mars_metrics_core::monitoring::record_size(
                        gst_mars_metrics_core::monitoring::Stage::Profiles,
                        data.len(),
                        self.pending as usize,
                    );
                    ReportData::RawPprof(data)
                },
            })
        }
        fn add_tag(&self, _: ThreadTag) -> pyroscope::Result<()> {
            Err(PyroscopeError::new(
                "use sample-time context publication, not thread tags",
            ))
        }
        fn remove_tag(&self, _: ThreadTag) -> pyroscope::Result<()> {
            Err(PyroscopeError::new(
                "use sample-time context publication, not thread tags",
            ))
        }
    }

    enum ProfileCommand {
        Flush(crossbeam_channel::Sender<Result<(), FlushError>>),
        Stop,
    }
    struct Worker {
        command: crossbeam_channel::Sender<ProfileCommand>,
        done: crossbeam_channel::Receiver<()>,
        handle: std::thread::JoinHandle<Result<(), FlushError>>,
    }
    impl Worker {
        fn start(
            sampler: Arc<Sampler>,
            config: PyroscopeConfig,
            timeout: Duration,
        ) -> Result<Self, String> {
            let (command, wake) = crossbeam_channel::bounded(1);
            let (finished, done) = crossbeam_channel::bounded::<()>(0);
            let (ready, started) = crossbeam_channel::bounded(1);
            let handle = std::thread::Builder::new()
                .name("mars-profile".into())
                .spawn(move || {
                    let _finished = finished;
                    let create = || {
                        super::upload::Uploader::start(
                            config.clone(),
                            timeout,
                            sampler.delivery.clone(),
                        )
                    };
                    let initial = create();
                    let _ = ready.send(initial.as_ref().map(|_| ()).map_err(Clone::clone));
                    let mut uploader = Some(initial?);
                    let mut backend = ProfileBackend::new(sampler.clone());
                    let mut next = std::time::Instant::now() + config.upload_interval;
                    let result = loop {
                        let command = match wake
                            .recv_timeout(next.saturating_duration_since(std::time::Instant::now()))
                        {
                            Ok(command) => Some(command),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                Some(ProfileCommand::Stop)
                            }
                        };
                        let report = backend
                            .report()
                            .map_err(|_| FlushError::ExportFailed)
                            .and_then(|batch| {
                                if backend.pending != 0 {
                                    uploader.as_ref().unwrap().submit(
                                        batch,
                                        backend.pending,
                                        command.is_some(),
                                    )?;
                                }
                                Ok(())
                            });
                        match command {
                            Some(ProfileCommand::Flush(reply)) => {
                                let delivered = uploader.take().unwrap().finish();
                                // Failed deliveries end this drain epoch, not sampling.
                                // Reopen even after rejection so a recovered server can
                                // accept later profiles; cumulative counters retain loss.
                                match create() {
                                    Ok(new) => uploader = Some(new),
                                    Err(error) => {
                                        let _ = reply.send(Err(error.clone()));
                                        break Err(error);
                                    }
                                }
                                let _ = reply.send(report.and(delivered));
                            }
                            Some(ProfileCommand::Stop) => break report,
                            None if report.is_err() => break report,
                            None => {}
                        }
                        next = std::time::Instant::now() + config.upload_interval;
                    };
                    sampler.stop();
                    #[cfg(feature = "gpu-profiles")]
                    {
                        // GPU leases may outlive the last CPU report. Publish
                        // quiescence before one final retirement pass: whichever
                        // worker releases its final lease last performs cleanup.
                        REPORTS_FINISHED.store(true, Release);
                        crate::profilecontext::after_report(crate::profilecontext::before_drain());
                    }
                    let delivered = uploader.map_or(Ok(()), |uploader| uploader.finish());
                    result.and(delivered)
                })
                .map_err(|e| e.to_string())?;
            if started.recv().map_err(|e| e.to_string())?.is_err() {
                let _ = handle.join();
                return Err("profile uploader initialization failed".into());
            }
            Ok(Self {
                command,
                done,
                handle,
            })
        }
        fn stop(self) -> Result<(), FlushError> {
            let _ = self.command.send(ProfileCommand::Stop);
            self.handle.join().map_err(|_| FlushError::ExportFailed)?
        }
    }

    struct Registration {
        sampler: &'static Sampler,
        index: usize,
    }
    impl Drop for Registration {
        fn drop(&mut self) {
            let _guard = self.sampler.registration.lock().unwrap();
            let slot = &self.sampler.slots[self.index];
            slot.publish(None);
            slot.element.store(0, Relaxed);
            slot.delete_timer();
            // Pending notifications to this exiting thread cannot sample a
            // recycled slot: the handler checks the target's native TID.
            slot.tid.store(0, Release);
        }
    }
    fn register(sampler: &'static Sampler) -> Result<Registration, String> {
        let _guard = sampler.registration.lock().unwrap();
        if !ACTIVE.load(Acquire) {
            return Err("sampler stopped".into());
        }
        let index = sampler
            .slots
            .iter()
            .position(|s| s.tid.load(Acquire) == 0)
            .ok_or("all 64 profiler thread slots are occupied")?;
        let slot = &sampler.slots[index];
        slot.prepare();
        slot.element.store(0, Relaxed);
        slot.correlate_element.store(true, Relaxed);
        // SAFETY: pthread attributes and signal masks below refer to the calling
        // thread, all output storage is initialized before use and destroyed.
        unsafe {
            let mut mask = std::mem::zeroed();
            if libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) != 0
                || libc::sigismember(&mask, sampler.signal) != 0
            {
                return Err("profiling signal is blocked on this thread".into());
            }
            let mut attr = std::mem::zeroed();
            if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
                return Err("cannot get native stack bounds".into());
            }
            let mut base = std::ptr::null_mut();
            let mut size = 0;
            let result = libc::pthread_attr_getstack(&attr, &mut base, &mut size);
            libc::pthread_attr_destroy(&mut attr);
            if result != 0 {
                return Err("cannot get native stack bounds".into());
            }
            let high = (base as usize)
                .checked_add(size)
                .ok_or("invalid stack bounds")?;
            slot.low.store(base as usize, Relaxed);
            slot.high.store(high, Relaxed);
            slot.publish(None);
            let tid = libc::syscall(libc::SYS_gettid) as i32;
            let mut event: libc::sigevent = std::mem::zeroed();
            event.sigev_notify = libc::SIGEV_THREAD_ID;
            event.sigev_notify_thread_id = tid;
            event.sigev_signo = sampler.signal;
            event.sigev_value.sival_ptr = (slot as *const Slot).cast_mut().cast();
            let mut timer = std::ptr::null_mut();
            if libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut event, &mut timer) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            slot.timer.store(timer as usize, Release);
            slot.tid.store(tid, Release);
            let nanos = 1_000_000_000 / sampler.rate;
            let interval = libc::timespec {
                tv_sec: (nanos / 1_000_000_000) as _,
                tv_nsec: (nanos % 1_000_000_000) as _,
            };
            let setting = libc::itimerspec {
                it_interval: interval,
                it_value: interval,
            };
            if libc::timer_settime(timer, 0, &setting, std::ptr::null_mut()) != 0 {
                let error = std::io::Error::last_os_error().to_string();
                slot.delete_timer();
                slot.tid.store(0, Release);
                return Err(error);
            }
        }
        Ok(Registration { sampler, index })
    }

    fn ids(token: &Token) -> [u64; 3] {
        let trace = token.trace_id.to_bytes();
        [
            u64::from_be_bytes(trace[..8].try_into().unwrap()),
            u64::from_be_bytes(trace[8..].try_into().unwrap()),
            u64::from_be_bytes(token.span_id.to_bytes()),
        ]
    }
    pub fn clear() {
        if ACTIVE.load(Relaxed) {
            let _ = REGISTRATION.try_with(|r| {
                if let Ok(r) = r.try_borrow() {
                    if let Some(r) = r.as_ref() {
                        r.sampler.slots[r.index].publish(None);
                    }
                }
            });
        }
    }
    pub fn publish(token: Option<&Token>) -> bool {
        if !ACTIVE.load(Relaxed) {
            return false;
        }
        REGISTRATION
            .try_with(|registration| {
                let mut registration = registration.borrow_mut();
                if registration.is_none() && ATTEMPTED.with(|v| !v.replace(true)) {
                    let sampler = STATE.get().unwrap().as_ref();
                    match register(sampler) {
                        Ok(r) => *registration = Some(r),
                        Err(error) => {
                            sampler.counters.registration_failures.fetch_add(1, Relaxed);
                            eprintln!("mars-otel profiles: thread registration failed: {error}");
                        }
                    }
                }
                if let Some(r) = registration.as_ref() {
                    r.sampler.slots[r.index].publish(token.map(ids));
                    token.is_some()
                } else {
                    false
                }
            })
            .unwrap_or(false)
    }

    // Register only once per native thread. Later calls leave the active span
    // and element context intact, including after a failed registration attempt.
    pub fn register_thread() {
        if ACTIVE.load(Relaxed) && ATTEMPTED.try_with(|v| !v.get()).unwrap_or(false) {
            publish(None);
        }
    }

    pub fn active() -> bool {
        ACTIVE.load(Relaxed)
    }
    pub fn publish_element(id: u64, correlate: bool) {
        if active() {
            let _ = REGISTRATION.try_with(|registration| {
                if let Ok(registration) = registration.try_borrow() {
                    if let Some(r) = registration.as_ref() {
                        let slot = &r.sampler.slots[r.index];
                        // Same-thread signal publication: clear IDs before
                        // entering an independently attributed worker scope.
                        slot.correlate_element.store(false, Relaxed);
                        slot.element.store(id, Relaxed);
                        slot.correlate_element.store(correlate, Relaxed);
                    }
                }
            });
        }
    }

    // Only reads the bounded snapshot, never follows pointers in application
    // memory. A bad/truncated chain retains its interrupted leaf PC.
    fn unwind(sample: &mut Sample, stack: &[u8], sp: usize, mut fp: usize) -> bool {
        while fp != 0 {
            let Some(offset) = fp.checked_sub(sp) else {
                return true;
            };
            let Some(pair) = offset
                .checked_add(16)
                .and_then(|end| stack.get(offset..end))
            else {
                return true;
            };
            if !fp.is_multiple_of(8) || sample.len == MAX_FRAMES {
                return true;
            }
            let next = usize::from_ne_bytes(pair[..8].try_into().unwrap());
            let ret = usize::from_ne_bytes(pair[8..].try_into().unwrap());
            if ret == 0 {
                return false;
            }
            let Some(pc) = arch::callsite(ret) else {
                return true;
            };
            sample.pcs[sample.len] = pc;
            sample.len += 1;
            if next == 0 {
                return false;
            }
            if next <= fp {
                return true;
            }
            fp = next;
        }
        false
    }

    extern "C" fn sample_signal(_: i32, info: *mut libc::siginfo_t, context: *mut c_void) {
        // SAFETY: SA_SIGINFO supplies these pointers. errno belongs to this
        // thread and must survive any syscalls made by the handler.
        unsafe {
            let errno = libc::__errno_location();
            let saved = *errno;
            if let Some(sampler) = STATE.get() {
                sampler.handlers.fetch_add(1, SeqCst);
                sample_interrupted(info, context);
                sampler.handlers.fetch_sub(1, Release);
            }
            *errno = saved;
        }
    }
    unsafe fn sample_interrupted(info: *mut libc::siginfo_t, context: *mut c_void) {
        if !ACTIVE.load(SeqCst) || info.is_null() || context.is_null() {
            return;
        }
        let Some(sampler) = STATE.get() else {
            return;
        };
        // SAFETY: caller obtained info/context from SA_SIGINFO. Validate the
        // timer's opaque address BEFORE dereferencing it; other users may send
        // this signal. Slot allocation is never freed or moved.
        let info = unsafe { &*info };
        if info.si_code != libc::SI_TIMER {
            return;
        }
        let address = unsafe { info.si_value().sival_ptr } as usize;
        let start = sampler.slots.as_ptr() as usize;
        let Some(offset) = address.checked_sub(start) else {
            return;
        };
        if offset % std::mem::size_of::<Slot>() != 0 {
            return;
        }
        let Some(slot) = sampler.slots.get(offset / std::mem::size_of::<Slot>()) else {
            return;
        };
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        if slot.tid.load(Acquire) != tid {
            return;
        }
        let timer = slot.timer.load(Acquire);
        if timer == NO_TIMER {
            return;
        }
        let overruns = unsafe { libc::timer_getoverrun(timer as libc::timer_t) };
        if overruns > 0 {
            sampler
                .counters
                .timer_overruns
                .fetch_add(overruns as u64, Relaxed);
        }
        let registers = unsafe { arch::registers(context.cast()) };
        let sp = registers.sp;
        let fp = registers.fp;
        let mut sample = Sample {
            ids: if slot.correlate_element.load(Relaxed) {
                slot.context()
            } else {
                [0; 3]
            },
            element: slot.element.load(Relaxed),
            tid,
            len: 1,
            ..Default::default()
        };
        sample.pcs[0] = registers.pc;
        // SAFETY: scratch is used only by this slot's non-reentrant handler.
        let Some(buffers) = slot.buffers.get() else {
            return;
        };
        let scratch = unsafe { &mut *buffers.scratch.get() };
        let available = slot.high.load(Relaxed).saturating_sub(sp).min(STACK_BYTES);
        let copied = if sp >= slot.low.load(Relaxed) && available > 0 {
            let local = libc::iovec {
                iov_base: scratch.as_mut_ptr().cast(),
                iov_len: available,
            };
            let remote = libc::iovec {
                iov_base: sp as *mut c_void,
                iov_len: available,
            };
            // The kernel validates application addresses. No speculative frame
            // pointer dereferences, allocator, libunwind or dynamic loader here.
            unsafe {
                libc::syscall(
                    libc::SYS_process_vm_readv,
                    sampler.pid,
                    &local,
                    1usize,
                    &remote,
                    1usize,
                    0usize,
                )
            }
        } else {
            -1
        };
        if copied <= 0 {
            sampler.counters.failed_stack_reads.fetch_add(1, Relaxed);
        } else if unwind(&mut sample, &scratch[..copied as usize], sp, fp) {
            sampler.counters.truncated_stacks.fetch_add(1, Relaxed);
        }
        if slot.push(sample) {
            sampler.counters.samples.fetch_add(1, Relaxed);
            if sample.element != 0 {
                if sample.element & 1 == 0 {
                    sampler.counters.element_samples.fetch_add(1, Relaxed);
                } else {
                    sampler.counters.task_samples.fetch_add(1, Relaxed);
                }
            }
            if sample.ids != [0; 3] {
                sampler.counters.correlated_samples.fetch_add(1, Relaxed);
            }
        } else {
            sampler.counters.dropped_full.fetch_add(1, Relaxed);
        }
    }

    pub fn initialize(
        resource: &opentelemetry_sdk::Resource,
        required: bool,
    ) -> Result<(), String> {
        let Some(url) = setting("URL").or_else(|| required.then(|| "http://localhost:4040".into()))
        else {
            return Ok(());
        };
        let url = url
            .into_string()
            .map_err(|_| "GST_OTEL_PROFILE_URL is not UTF-8")?;
        let parsed = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("GST_OTEL_PROFILE_URL must be an HTTP(S) URL".into());
        }
        let rate = setting("HZ")
            .map(|v| v.into_string().map_err(|_| "profile HZ is not UTF-8"))
            .transpose()?
            .unwrap_or_else(|| "100".into())
            .parse::<u32>()
            .map_err(|_| "GST_OTEL_PROFILE_HZ must be 1..=1000")?;
        if !(1..=1000).contains(&rate) {
            return Err("GST_OTEL_PROFILE_HZ must be 1..=1000".into());
        }
        let timeout = setting("TIMEOUT_MS")
            .map(|v| {
                v.into_string()
                    .map_err(|_| "profile TIMEOUT_MS is not UTF-8")
            })
            .transpose()?
            .unwrap_or_else(|| "10000".into())
            .parse::<u64>()
            .map_err(|_| "profile TIMEOUT_MS must be 1..=60000")?;
        if !(1..=60000).contains(&timeout) {
            return Err("profile TIMEOUT_MS must be 1..=60000".into());
        }
        crate::profilecontext::configure()?;
        let symbolization = setting("SYMBOLIZATION")
            .map(|v| {
                v.into_string()
                    .map_err(|_| "profile SYMBOLIZATION is not UTF-8")
            })
            .transpose()?
            .as_deref()
            .unwrap_or("full")
            .parse()?;
        let signal = setting("SIGNAL")
            .map(|v| v.into_string().map_err(|_| "profile SIGNAL is not UTF-8"))
            .transpose()?
            .map(|s| s.parse::<i32>())
            .transpose()
            .map_err(|_| "invalid GST_OTEL_PROFILE_SIGNAL")?
            .unwrap_or_else(|| libc::SIGRTMIN() + 6);
        if !(libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signal) {
            return Err("profile signal must be a realtime signal".into());
        }
        // SAFETY: valid signal; query before reserving. Applications must not
        // concurrently replace this disposition or use this reserved signal.
        unsafe {
            let mut previous: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut previous) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            if previous.sa_sigaction != libc::SIG_DFL {
                return Err("profile signal already has a handler".into());
            }
            let mut sampler = Sampler::new(rate, signal);
            sampler.symbolization = symbolization;
            let state = Arc::new(sampler);
            STATE
                .set(state.clone())
                .map_err(|_| "embedded profiler already initialized")?;
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = sample_signal as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let name = resource
                .iter()
                .find(|(k, _)| k.as_str() == "service.name")
                .map(|(_, v)| v.to_string())
                .unwrap_or_else(|| "mars-otel".into());
            let config = PyroscopeConfig::new(
                url,
                name,
                rate,
                "mars-otel-embedded",
                env!("CARGO_PKG_VERSION"),
            )
            .upload_interval(Duration::from_secs(1));
            *WORKER.lock().unwrap() = Some(Worker::start(
                state,
                config,
                Duration::from_millis(timeout),
            )?);
            ACTIVE.store(true, Release);
        }
        Ok(())
    }

    pub fn flush(timeout: Duration) -> Result<(), FlushError> {
        if STATE.get().is_none() {
            return Ok(());
        }
        let start = std::time::Instant::now();
        let (command, done) = {
            let worker = WORKER.lock().unwrap();
            let worker = worker.as_ref().ok_or(FlushError::Closed)?;
            (worker.command.clone(), worker.done.clone())
        };
        let (reply, result) = crossbeam_channel::bounded(1);
        command
            .send_timeout(
                ProfileCommand::Flush(reply),
                timeout.saturating_sub(start.elapsed()),
            )
            .map_err(|e| {
                if e.is_timeout() {
                    FlushError::Timeout
                } else {
                    FlushError::Closed
                }
            })?;
        crossbeam_channel::select! {
            recv(result) -> result => result.map_err(|_| FlushError::Closed)?,
            recv(done) -> _ => Err(FlushError::Closed),
            default(timeout.saturating_sub(start.elapsed())) => Err(FlushError::Timeout),
        }
    }

    pub fn shutdown(timeout: Duration) -> Result<(), FlushError> {
        let Some(state) = STATE.get() else {
            return Ok(());
        };
        let start = std::time::Instant::now();
        ACTIVE.store(false, SeqCst);
        if !STOPPING.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let worker = WORKER.lock().unwrap().take();
            // Uploads can wait for queued HTTP requests. Never impose
            // that unbounded wait on the caller of our deadline-based API.
            std::thread::spawn(move || {
                // Registration and handler quiescence also run off the caller:
                // a preempted registering thread must not extend its deadline.
                state.stop();
                let result = if let Some(worker) = worker {
                    worker.stop()
                } else {
                    Ok(())
                };
                *STOPPED.0.lock().unwrap() = Some(result);
                STOPPED.1.notify_all();
            });
        }
        let result = STOPPED.0.lock().unwrap();
        let (result, _) = STOPPED
            .1
            .wait_timeout_while(result, timeout.saturating_sub(start.elapsed()), |r| {
                r.is_none()
            })
            .unwrap();
        result.clone().unwrap_or(Err(FlushError::Timeout))
    }
    #[cfg(feature = "gpu-profiles")]
    pub fn reports_finished() -> bool {
        STATE.get().is_none() || REPORTS_FINISHED.load(Acquire)
    }
    pub fn stats() -> ProfileStats {
        let Some(s) = STATE.get() else {
            return ProfileStats::default();
        };
        ProfileStats {
            samples: s.counters.samples.load(Relaxed),
            correlated_samples: s.counters.correlated_samples.load(Relaxed),
            element_samples: s.counters.element_samples.load(Relaxed),
            task_samples: s.counters.task_samples.load(Relaxed),
            context_capacity_rejections: crate::profilecontext::rejected(),
            context_descriptions: crate::profilecontext::retained(),
            context_retired: crate::profilecontext::retired(),
            dropped_full: s.counters.dropped_full.load(Relaxed),
            timer_overruns: s.counters.timer_overruns.load(Relaxed),
            failed_stack_reads: s.counters.failed_stack_reads.load(Relaxed),
            truncated_stacks: s.counters.truncated_stacks.load(Relaxed),
            registration_failures: s.counters.registration_failures.load(Relaxed),
            reported_samples: s.delivery.reported_samples.load(Relaxed),
            dropped_processing: s.counters.dropped_processing.load(Relaxed),
            dropped_upload: s.delivery.dropped_upload.load(Relaxed),
            accepted_samples: s.delivery.accepted_samples.load(Relaxed),
            failed_samples: s.delivery.failed_samples.load(Relaxed),
            accepted_profiles: s.delivery.accepted_profiles.load(Relaxed),
            failed_exports: s.delivery.failed_exports.load(Relaxed),
            pending_samples: s.delivery.reported_samples.load(Relaxed).saturating_sub(
                s.delivery.accepted_samples.load(Relaxed) + s.delivery.failed_samples.load(Relaxed),
            ),
            pending_profiles: s.delivery.reported_profiles.load(Relaxed).saturating_sub(
                s.delivery.accepted_profiles.load(Relaxed)
                    + s.delivery.failed_exports.load(Relaxed),
            ),
            active_threads: s
                .slots
                .iter()
                .filter(|s| s.timer.load(Relaxed) != NO_TIMER)
                .count() as u64,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn capture_context_edges_thread_retirement_and_shutdown_are_isolated() {
            use crate::{
                capture,
                export::{
                    tests::{runtime, Collector},
                    Config,
                },
            };
            use gstreamer::{self as gst, prelude::*};
            if std::env::var_os("GST_OTEL_PROFILE_UNIT_CHILD").is_none() {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                for (key, _) in std::env::vars().filter(|(k, _)| {
                    k.starts_with("GST_") || k.starts_with("OTEL_") || k.starts_with("MARS_")
                }) {
                    child.env_remove(key);
                }
                use std::io::Read;
                let mut child = child.args(["--exact", "profiles::linux::tests::capture_context_edges_thread_retirement_and_shutdown_are_isolated", "--nocapture"])
                    .env("GST_OTEL_PROFILE_UNIT_CHILD", "1")
                    .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
                let mut stdout = child.stdout.take().unwrap();
                let mut stderr = child.stderr.take().unwrap();
                let stdout = std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    stdout.read_to_end(&mut bytes).unwrap();
                    bytes
                });
                let stderr = std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    stderr.read_to_end(&mut bytes).unwrap();
                    bytes
                });
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status;
                    }
                    if std::time::Instant::now() >= deadline {
                        child.kill().unwrap();
                        break child.wait().unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let output = std::process::Output {
                    status,
                    stdout: stdout.join().unwrap(),
                    stderr: stderr.join().unwrap(),
                };
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            std::env::set_var(
                "GST_OTEL_PROFILE_URL",
                format!("http://{}", server.server_addr()),
            );
            std::env::set_var("GST_OTEL_PROFILE_HZ", "1000");
            // This stress test covers capture and quiescence. Full DWARF
            // loading is tested in profilesymbols and can dominate its deadline
            // when the test binary is large or the machine is under load.
            std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "functions");
            let done = Arc::new(AtomicBool::new(false));
            let stop = done.clone();
            let receiver = std::thread::spawn(move || {
                while !stop.load(Relaxed) {
                    if let Some(request) = server.recv_timeout(Duration::from_millis(10)).unwrap() {
                        request.respond(tiny_http::Response::empty(200)).unwrap();
                    }
                }
            });
            gst::init().unwrap();
            initialize(&opentelemetry_sdk::Resource::builder_empty().build(), false).unwrap();
            let published = || {
                REGISTRATION.with(|r| {
                    r.borrow()
                        .as_ref()
                        .map(|r| r.sampler.slots[r.index].context())
                        .unwrap_or([0; 3])
                })
            };
            let source = gst::ElementFactory::make("fakesrc").build().unwrap();
            let pad = source.static_pad("src").unwrap();
            let other = gst::Pad::builder(gst::PadDirection::Src).build();
            let buffer = gst::Buffer::new();
            let collector = Collector::default();
            let runtime = runtime(Config::default(), collector.clone());
            capture::begin(&runtime, 100, &pad, &buffer);
            let outer = published();
            assert_ne!(outer, [0; 3]);
            capture::begin(&runtime, 101, &other, &buffer);
            assert_ne!(published(), outer);
            assert_ne!(published(), [0; 3]);
            capture::end(&runtime, 102, &other, 0);
            assert_eq!(published(), outer);
            capture::end(&runtime, 103, &pad, 0);
            assert_eq!(published(), [0; 3]);

            // Follow capture's configured depth bound without duplicating it.
            let mut depth = 0;
            while runtime.stats.snapshot().dropped_depth == 0 {
                capture::begin(&runtime, 200, &pad, &buffer);
                depth += 1;
                assert!(depth < 1024);
            }
            assert_eq!(published(), [0; 3], "overflow must suppress the parent ID");
            capture::end(&runtime, 201, &pad, 0);
            assert_ne!(published(), [0; 3], "balanced overflow restores the parent");
            for _ in 1..depth {
                capture::end(&runtime, 202, &pad, 0);
            }
            assert_eq!(published(), [0; 3]);

            capture::begin(&runtime, 300, &pad, &buffer);
            capture::end(&runtime, 301, &other, 0); // unmatched post
            assert_eq!(published(), [0; 3]);
            capture::begin(&runtime, 302, &other, &buffer);
            assert_eq!(
                published(),
                [0; 3],
                "suppress until uncertain nesting drains"
            );
            capture::end(&runtime, 303, &other, 0);
            capture::end(&runtime, 304, &pad, 0);
            capture::begin(&runtime, 305, &pad, &buffer);
            assert_ne!(published(), [0; 3]);
            runtime.shutdown(Duration::from_secs(2)).unwrap();
            capture::begin(&runtime, 306, &other, &buffer);
            assert_eq!(
                published(),
                [0; 3],
                "closed runtime must not publish a new span"
            );
            capture::end(&runtime, 307, &other, 0);
            capture::end(&runtime, 308, &pad, 0);
            assert_eq!(published(), [0; 3]);
            assert!(collector.0.lock().unwrap().iter().any(|s| s
                .attributes
                .iter()
                .any(|a| a.key.as_str() == "pyroscope.profile.id")));

            for _ in 0..MAX_THREADS + 16 {
                std::thread::spawn(|| {
                    publish(None);
                    REGISTRATION.with(|r| assert!(r.borrow().is_some()));
                })
                .join()
                .unwrap();
            }
            assert_eq!(stats().registration_failures, 0);
            assert_eq!(
                stats().active_threads,
                1,
                "retired threads must release timers and slots"
            );

            // Real high-rate signals during allocation, application mutex use,
            // loader activity and thread retirement. The parent has a watchdog.
            let mutex = &Mutex::new(());
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(move || {
                        publish(None);
                        let until = std::time::Instant::now() + Duration::from_secs(2);
                        while std::time::Instant::now() < until {
                            let _lock = mutex.lock().unwrap();
                            std::hint::black_box(vec![42u8; 32 * 1024]);
                            // SAFETY: valid system library and paired close;
                            // no symbol obtained from it outlives the handle.
                            unsafe {
                                let handle = libc::dlopen(
                                    c"libm.so.6".as_ptr(),
                                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                                );
                                assert!(!handle.is_null());
                                libc::dlclose(handle);
                            }
                        }
                    });
                }
            });
            assert!(stats().samples > 0);
            assert_eq!(stats().registration_failures, 0);

            // Even a stalled registration cannot hold the shutdown caller past
            // its deadline. Cleanup resumes when the registration lock clears.
            let lock = STATE.get().unwrap().registration.lock().unwrap();
            let start = std::time::Instant::now();
            assert_eq!(shutdown(Duration::from_millis(5)), Err(FlushError::Timeout));
            assert!(start.elapsed() < Duration::from_secs(1));
            drop(lock);
            shutdown(Duration::from_secs(5)).unwrap();
            assert_eq!(stats().active_threads, 0);
            let stats = stats();
            assert_eq!(
                stats.samples,
                stats.reported_samples + stats.dropped_upload + stats.dropped_processing
            );
            done.store(true, Relaxed);
            receiver.join().unwrap();
        }

        #[test]
        fn ring_is_bounded_and_preserves_queued_context_across_slot_reuse() {
            let slot = Slot::new();
            assert!(slot.buffers.get().is_none());
            slot.prepare();
            for id in 1..=CAPACITY {
                assert!(slot.push(Sample {
                    ids: [1, 2, id as u64],
                    ..Default::default()
                }));
            }
            assert!(!slot.push(Sample::default()));
            slot.publish(Some([3, 4, 5])); // changing active context cannot relabel old samples
            let mut actual = Vec::new();
            slot.drain(&mut |s| actual.push(s.ids[2]));
            assert_eq!(actual, (1..=CAPACITY as u64).collect::<Vec<_>>());
            assert!(slot.push(Sample::default()));
        }

        #[test]
        fn concurrent_ring_transfers_complete_samples_in_order() {
            let slot = Arc::new(Slot::new());
            slot.prepare();
            let producer = slot.clone();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let thread = std::thread::spawn(move || {
                for id in 1..=20000 {
                    let sample = Sample {
                        ids: [id, id + 1, id + 2],
                        element: id * 2,
                        tid: id as i32,
                        len: 2,
                        pcs: [id as usize; MAX_FRAMES],
                    };
                    while !producer.push(sample) {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "ring producer stalled"
                        );
                        std::thread::yield_now();
                    }
                }
            });
            let mut expected = 1;
            while expected <= 20000 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "ring consumer stalled"
                );
                slot.drain(&mut |sample| {
                    assert_eq!(sample.ids, [expected, expected + 1, expected + 2]);
                    assert_eq!(sample.element, expected * 2);
                    assert_eq!(sample.tid, expected as i32);
                    assert_eq!(sample.len, 2);
                    assert_eq!(sample.pcs, [expected as usize; MAX_FRAMES]);
                    expected += 1;
                });
                std::thread::yield_now();
            }
            thread.join().unwrap();
        }

        #[test]
        fn snapshot_unwinding_stops_on_invalid_cyclic_and_truncated_chains() {
            let mut stack = [0u8; 32];
            stack[..8].copy_from_slice(&1016usize.to_ne_bytes());
            stack[8..16].copy_from_slice(&2000usize.to_ne_bytes());
            stack[24..32].copy_from_slice(&3000usize.to_ne_bytes());
            let mut sample = Sample {
                len: 1,
                ..Default::default()
            };
            assert!(!unwind(&mut sample, &stack, 1000, 1000));
            assert_eq!(
                &sample.pcs[1..sample.len],
                &[arch::callsite(2000).unwrap(), arch::callsite(3000).unwrap()]
            );
            for bad in [999, 1001, usize::MAX] {
                assert!(unwind(&mut Sample::default(), &stack, 1000, bad));
            }
            stack[..8].copy_from_slice(&1000usize.to_ne_bytes());
            assert!(unwind(&mut Sample::default(), &stack, 1000, 1000));
        }

        #[test]
        fn missing_or_incomplete_frames_preserve_the_interrupted_leaf() {
            for fp in [0, 999, 1000, usize::MAX] {
                let mut sample = Sample {
                    len: 1,
                    ..Default::default()
                };
                sample.pcs[0] = 0x1234;
                let truncated = unwind(&mut sample, &[0; 8], 1000, fp);
                assert_eq!(truncated, fp != 0);
                assert_eq!(&sample.pcs[..sample.len], &[0x1234]);
            }
        }
        #[test]
        fn aggregation_and_pprof_keep_identical_stacks_with_different_span_ids_separate() {
            let sampler = Arc::new(Sampler::new(100, libc::SIGRTMIN() + 6));
            sampler.slots[0].prepare();
            for span in [11, 11, 12] {
                assert!(sampler.slots[0].push(Sample {
                    ids: [1, 2, span],
                    element: 0,
                    tid: 7,
                    len: 1,
                    pcs: [0; MAX_FRAMES]
                }));
            }
            let mut backend = ProfileBackend::new(sampler);
            let ReportData::RawPprof(bytes) = backend.report().unwrap().data else {
                panic!()
            };
            let profile = Profile::decode(bytes.as_slice()).unwrap();
            let mut values = HashMap::new();
            for sample in profile.sample {
                let span = sample
                    .label
                    .iter()
                    .find(|l| profile.string_table[l.key as usize] == "span_id")
                    .unwrap();
                values.insert(
                    profile.string_table[span.str as usize].clone(),
                    sample.value[0],
                );
            }
            assert_eq!(values["000000000000000b"], 20_000_000);
            assert_eq!(values["000000000000000c"], 10_000_000);
            assert_eq!(backend.pending, 3);
        }
    }
}
