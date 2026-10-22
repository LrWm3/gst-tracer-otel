//! Experimental embedded CPU sampling and the Pyroscope delivery adapter.
//!
//! Review boundary: all signal handling, context publication, sample ownership,
//! aggregation and profiler lifecycle live here. Capture only publishes fixed
//! IDs. This module deliberately does NOT call Pyroscope's thread-tag API.
//!
//! Linux x86-64 only. Each participating thread owns a CPU timer and an SPSC
//! sample ring. The signal handler copies registers and a bounded stack window
//! using process_vm_readv, walks frame pointers in that COPY, and snapshots IDs.
//! It never borrows Rust TLS, allocates, locks, symbolizes or calls Pyroscope.
//! A report worker drains rings, symbolizes files with a bounded cache, and
//! supplies compact pprof to Pyroscope's bounded SDK upload queue.
//!
//! Slots are retained for process lifetime because deleted timers can leave
//! pending signals. GStreamer makes loaded plugin modules resident. The reserved
//! realtime signal remains installed after shutdown; it becomes a no-op. This
//! prototype does not support fork after initialization or unloading an rlib's
//! hosting DSO. Normal GStreamer plugin loading is supported.

use std::time::Duration;

use crate::{capture::Token, export::FlushError};

/// Sampling counters. `reported_samples` means handed to the Pyroscope SDK,
/// whose uploader does not expose delivery acknowledgements through Backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileStats {
    pub samples: u64,
    pub correlated_samples: u64,
    pub dropped_full: u64,
    pub timer_overruns: u64,
    pub failed_stack_reads: u64,
    pub truncated_stacks: u64,
    pub registration_failures: u64,
    pub reported_samples: u64,
    pub dropped_processing: u64,
    pub dropped_upload: u64,
    pub active_threads: u64,
}

#[cfg(all(feature = "profiles", target_os = "linux", target_arch = "x86_64"))]
use linux as platform;

#[cfg(not(all(feature = "profiles", target_os = "linux", target_arch = "x86_64")))]
mod platform {
    use super::*;
    pub fn initialize(_: &opentelemetry_sdk::Resource) -> Result<(), String> {
        if std::env::var_os("GST_OTEL_PROFILE_URL").is_some() {
            Err("requires the profiles Cargo feature on Linux x86-64".into())
        } else {
            Ok(())
        }
    }
    pub fn publish(_: Option<&Token>) -> bool {
        false
    }
    pub fn clear() {}
    pub fn shutdown(_: Duration) -> Result<(), FlushError> {
        Ok(())
    }
    pub fn stats() -> ProfileStats {
        ProfileStats::default()
    }
}

pub(crate) use platform::{clear, initialize, publish, shutdown};

pub fn profile_stats() -> ProfileStats {
    platform::stats()
}

#[cfg(all(feature = "profiles", target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use prost::Message;
    use pyroscope::{
        backend::{Backend, ReportBatch, ReportData, StackFrame, ThreadTag},
        encode::gen::google::{
            Function, Label, Line, Location, Profile, Sample as PprofSample, ValueType,
        },
        error::PyroscopeError,
        pyroscope::PyroscopeConfig,
        session::{Session, SessionManager, SessionSignal},
    };
    use std::{
        cell::{RefCell, UnsafeCell},
        collections::HashMap,
        ffi::c_void,
        fs::File,
        os::unix::fs::MetadataExt,
        path::PathBuf,
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
        tid: i32,
        len: usize,
        pcs: [usize; MAX_FRAMES],
    }
    impl Default for Sample {
        fn default() -> Self {
            Self {
                ids: [0; 3],
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
        dropped_full: AtomicU64,
        timer_overruns: AtomicU64,
        failed_stack_reads: AtomicU64,
        truncated_stacks: AtomicU64,
        registration_failures: AtomicU64,
        reported_samples: AtomicU64,
        dropped_processing: AtomicU64,
        dropped_upload: AtomicU64,
    }
    struct Sampler {
        slots: Box<[Slot]>,
        registration: Mutex<()>,
        counters: Counters,
        signal: i32,
        rate: u32,
        pid: i32,
        handlers: AtomicUsize,
    }
    impl Sampler {
        fn new(rate: u32, signal: i32) -> Self {
            Self {
                slots: (0..MAX_THREADS).map(|_| Slot::new()).collect(),
                registration: Mutex::new(()),
                counters: Counters::default(),
                rate,
                signal,
                // SAFETY: getpid has no preconditions.
                pid: unsafe { libc::getpid() },
                handlers: AtomicUsize::new(0),
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

    const MAX_SYMBOLS: usize = 4096;
    const SYMBOL_BYTES: usize = 4 * 1024 * 1024;
    const MAX_PROFILE_BYTES: usize = 8 * 1024 * 1024;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Mapping {
        start: usize,
        end: usize,
        offset: u64,
        device: u64,
        inode: u64,
        path: PathBuf,
    }
    impl Mapping {
        fn parse(line: &str) -> Option<Self> {
            let mut rest = line;
            let mut field = || {
                rest = rest.trim_start();
                let (value, remaining) =
                    rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len()));
                rest = remaining;
                value
            };
            let (start, end) = field().split_once('-')?;
            if !field().contains('x') {
                return None;
            }
            let offset = u64::from_str_radix(field(), 16).ok()?;
            let (major, minor) = field().split_once(':')?;
            let device = libc::makedev(
                u32::from_str_radix(major, 16).ok()?,
                u32::from_str_radix(minor, 16).ok()?,
            );
            let inode = field().parse().ok()?;
            let path = rest.trim_start();
            if !path.starts_with('/') || path.ends_with(" (deleted)") {
                return None;
            }
            // proc escapes newlines; spaces (including consecutive spaces) are literal.
            let path = PathBuf::from(path.replace("\\012", "\n"));
            Some(Self {
                start: usize::from_str_radix(start, 16).ok()?,
                end: usize::from_str_radix(end, 16).ok()?,
                offset,
                device,
                inode,
                path,
            })
        }
        fn open(&self) -> Option<File> {
            let file = File::open(&self.path).ok()?;
            let metadata = file.metadata().ok()?;
            // Do not symbolize against a replacement file at the same path.
            (metadata.ino() == self.inode && metadata.dev() == self.device).then_some(file)
        }
    }

    #[derive(Default)]
    struct Symbols {
        mappings: Vec<Mapping>,
        frames: HashMap<usize, Vec<StackFrame>>,
        bytes: usize,
    }
    impl Symbols {
        fn refresh(&mut self, pcs: &[usize]) {
            let mappings: Vec<_> = std::fs::read_to_string("/proc/self/maps")
                .unwrap_or_default()
                .lines()
                .filter_map(Mapping::parse)
                .collect();
            if self.mappings != mappings {
                self.frames.clear();
                self.bytes = 0;
                self.mappings = mappings;
            }
            // Evict between reports, never while encoding a report. Bound both
            // the address count and retained strings/inline frames.
            if self.frames.len()
                + pcs
                    .iter()
                    .filter(|pc| !self.frames.contains_key(pc))
                    .count()
                > MAX_SYMBOLS
            {
                self.frames.clear();
                self.bytes = 0;
            }
            // PCs are sorted, so each ELF/DWARF context is built once per report
            // and dropped before loading the next file. No global resolver lock,
            // libunwind, dladdr, or dl_iterate_phdr (which takes the loader lock).
            for mapping in &self.mappings {
                let first = pcs.partition_point(|pc| *pc < mapping.start);
                let last = pcs.partition_point(|pc| *pc < mapping.end);
                let missing: Vec<_> = pcs[first..last]
                    .iter()
                    .copied()
                    .filter(|pc| !self.frames.contains_key(pc))
                    .collect();
                if missing.is_empty()
                    || self.frames.len() == MAX_SYMBOLS
                    || self.bytes >= SYMBOL_BYTES
                {
                    continue;
                }
                let Some(file) = mapping.open() else { continue };
                use object::{Object, ObjectSegment};
                let cache = object::read::ReadCache::new(&file);
                let Ok(object) = object::File::parse(&cache) else {
                    continue;
                };
                let segments: Vec<_> = object
                    .segments()
                    .map(|s| (s.file_range(), s.address()))
                    .collect();
                // Prefer the installed build-id debug file when available. The
                // open fd keeps the original inode stable while Loader opens it.
                use std::os::fd::AsRawFd;
                let binary = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
                let debug = object
                    .build_id()
                    .ok()
                    .flatten()
                    .filter(|id| id.len() >= 2)
                    .and_then(|id| {
                        let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
                        let path = PathBuf::from(format!(
                            "/usr/lib/debug/.build-id/{}/{}.debug",
                            &id[..2],
                            &id[2..]
                        ));
                        let debug_file = File::open(&path).ok()?;
                        let debug_cache = object::read::ReadCache::new(debug_file);
                        let debug_object = object::File::parse(&debug_cache).ok()?;
                        (debug_object.build_id().ok()? == object.build_id().ok()?).then_some(path)
                    });
                let loader = debug
                    .as_ref()
                    .and_then(|p| addr2line::Loader::new(p).ok())
                    .or_else(|| addr2line::Loader::new(&binary).ok());
                for pc in missing {
                    if self.frames.len() == MAX_SYMBOLS {
                        break;
                    }
                    let offset = mapping.offset + (pc - mapping.start) as u64;
                    let address = segments.iter().find_map(|&((start, len), address)| {
                        let relative = offset.checked_sub(start)?;
                        (relative < len).then(|| address + relative)
                    });
                    let frames = resolve_file(pc, address, loader.as_ref());
                    let bytes = frames.capacity() * std::mem::size_of::<StackFrame>()
                        + frames
                            .iter()
                            .map(|f| {
                                f.name.as_ref().map_or(0, String::capacity)
                                    + f.filename.as_ref().map_or(0, String::capacity)
                            })
                            .sum::<usize>();
                    if self.bytes + bytes > SYMBOL_BYTES {
                        break;
                    }
                    self.bytes += bytes;
                    self.frames.insert(pc, frames);
                }
            }
        }
    }

    fn short_string(value: &str) -> String {
        let mut end = value.len().min(1024);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value[..end].to_owned()
    }

    fn resolve_file(
        pc: usize,
        address: Option<u64>,
        loader: Option<&addr2line::Loader>,
    ) -> Vec<StackFrame> {
        let mut frames = Vec::new();
        if let (Some(address), Some(loader)) = (address, loader) {
            if let Ok(mut resolved) = loader.find_frames(address) {
                while let Ok(Some(frame)) = resolved.next() {
                    frames.push(StackFrame {
                        name: frame
                            .function
                            .and_then(|f| f.demangle().ok().map(|v| short_string(&v))),
                        filename: frame
                            .location
                            .as_ref()
                            .and_then(|l| l.file.map(short_string)),
                        line: frame.location.and_then(|l| l.line),
                        ..Default::default()
                    });
                    if frames.len() == 16 {
                        break;
                    }
                }
            }
            if frames.is_empty() {
                if let Some(name) = loader.find_symbol(address) {
                    frames.push(StackFrame {
                        name: Some(short_string(&addr2line::demangle_auto(name.into(), None))),
                        ..Default::default()
                    });
                }
            }
        }
        if frames.is_empty() {
            frames.push(StackFrame {
                name: Some(format!("0x{pc:x}")),
                ..Default::default()
            });
        }
        frames
    }

    // Intern once per report. The upload queue holds protobuf bytes, not a copy
    // of every resolved frame/string for every distinct span/stack combination.
    struct Pprof {
        profile: Profile,
        strings: HashMap<String, i64>,
        functions: HashMap<(i64, i64), u64>,
        locations: HashMap<usize, u64>,
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
            self.profile.sample.push(PprofSample {
                location_id: sample.pcs[..sample.len]
                    .iter()
                    .map(|pc| self.locations[pc])
                    .collect(),
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
            Self {
                sampler,
                symbols: Symbols::default(),
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
            Ok(ReportBatch {
                profile_type: "process_cpu".into(),
                data: ReportData::RawPprof(data),
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

    struct Worker {
        stop: std::sync::mpsc::Sender<()>,
        handle: std::thread::JoinHandle<Result<(), FlushError>>,
    }
    impl Worker {
        fn start(sampler: Arc<Sampler>, config: PyroscopeConfig) -> Result<Self, String> {
            let (stop, wake) = std::sync::mpsc::channel();
            let handle = std::thread::Builder::new()
                .name("mars-profile".into())
                .spawn(move || {
                    let mut backend = ProfileBackend::new(sampler);
                    let manager = SessionManager::new().map_err(|_| FlushError::ExportFailed)?;
                    let mut next = std::time::Instant::now() + config.upload_interval;
                    let result = loop {
                        let stopping = !matches!(
                            wake.recv_timeout(
                                next.saturating_duration_since(std::time::Instant::now())
                            ),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        );
                        let batch = match backend.report() {
                            Ok(batch) => batch,
                            Err(_) => break Err(FlushError::ExportFailed),
                        };
                        if backend.pending != 0 {
                            // Raw pprof carries its own timestamps. Session's integer
                            // range is unused by the SDK for this backend format.
                            let session = Session {
                                config: config.clone(),
                                batch,
                                from: 0,
                                until: 0,
                            };
                            if let Err(error) = enqueue(
                                &manager.tx,
                                session,
                                backend.pending,
                                &backend.sampler.counters,
                            ) {
                                break Err(error);
                            }
                        }
                        if stopping {
                            break Ok(());
                        }
                        // No unbounded timer-message backlog after a slow report.
                        next += config.upload_interval;
                        if next < std::time::Instant::now() {
                            next = std::time::Instant::now();
                        }
                    };
                    // Closing the only sender drains the SDK's bounded upload queue.
                    // No kill-message send can block behind a full queue. Unlike the
                    // Agent stop condvar, join also observes failure/panic/early exit.
                    drop(manager.tx);
                    let uploaded = manager
                        .handle
                        .unwrap()
                        .join()
                        .map_err(|_| FlushError::ExportFailed)?
                        .map_err(|_| FlushError::ExportFailed);
                    result.and(uploaded)
                })
                .map_err(|e| e.to_string())?;
            Ok(Self { stop, handle })
        }
        fn stop(self) -> Result<(), FlushError> {
            let _ = self.stop.send(());
            self.handle.join().map_err(|_| FlushError::ExportFailed)?
        }
    }
    fn enqueue(
        tx: &std::sync::mpsc::SyncSender<SessionSignal>,
        session: Session,
        count: u64,
        counters: &Counters,
    ) -> Result<(), FlushError> {
        match tx.try_send(SessionSignal::Session(Box::new(session))) {
            Ok(()) => {
                counters.reported_samples.fetch_add(count, Relaxed);
                Ok(())
            }
            Err(error) => {
                counters.dropped_upload.fetch_add(count, Relaxed);
                match error {
                    std::sync::mpsc::TrySendError::Full(_) => Ok(()),
                    std::sync::mpsc::TrySendError::Disconnected(_) => Err(FlushError::ExportFailed),
                }
            }
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
            if fp % 8 != 0 || sample.len == MAX_FRAMES {
                return true;
            }
            let next = usize::from_ne_bytes(pair[..8].try_into().unwrap());
            let ret = usize::from_ne_bytes(pair[8..].try_into().unwrap());
            if ret == 0 {
                return false;
            }
            sample.pcs[sample.len] = ret.saturating_sub(1);
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
        let registers = unsafe { &(*(context as *const libc::ucontext_t)).uc_mcontext.gregs };
        let sp = registers[libc::REG_RSP as usize] as usize;
        let fp = registers[libc::REG_RBP as usize] as usize;
        let mut sample = Sample {
            ids: slot.context(),
            tid,
            len: 1,
            ..Default::default()
        };
        sample.pcs[0] = registers[libc::REG_RIP as usize] as usize;
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
            if sample.ids != [0; 3] {
                sampler.counters.correlated_samples.fetch_add(1, Relaxed);
            }
        } else {
            sampler.counters.dropped_full.fetch_add(1, Relaxed);
        }
    }

    pub fn initialize(resource: &opentelemetry_sdk::Resource) -> Result<(), String> {
        let Some(url) = std::env::var_os("GST_OTEL_PROFILE_URL") else {
            return Ok(());
        };
        let url = url
            .into_string()
            .map_err(|_| "GST_OTEL_PROFILE_URL is not UTF-8")?;
        let parsed = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("GST_OTEL_PROFILE_URL must be an HTTP(S) URL".into());
        }
        let rate = std::env::var("GST_OTEL_PROFILE_HZ")
            .unwrap_or_else(|_| "100".into())
            .parse::<u32>()
            .map_err(|_| "GST_OTEL_PROFILE_HZ must be 1..=1000")?;
        if !(1..=1000).contains(&rate) {
            return Err("GST_OTEL_PROFILE_HZ must be 1..=1000".into());
        }
        let signal = std::env::var("GST_OTEL_PROFILE_SIGNAL")
            .ok()
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
            let state = Arc::new(Sampler::new(rate, signal));
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
            *WORKER.lock().unwrap() = Some(Worker::start(state, config)?);
            ACTIVE.store(true, Release);
        }
        Ok(())
    }

    pub fn shutdown(timeout: Duration) -> Result<(), FlushError> {
        let Some(state) = STATE.get() else {
            return Ok(());
        };
        let start = std::time::Instant::now();
        ACTIVE.store(false, SeqCst);
        if !STOPPING.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let worker = WORKER.lock().unwrap().take();
            // SDK uploads can wait for queued HTTP requests. Never impose
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
    pub fn stats() -> ProfileStats {
        let Some(s) = STATE.get() else {
            return ProfileStats::default();
        };
        ProfileStats {
            samples: s.counters.samples.load(Relaxed),
            correlated_samples: s.counters.correlated_samples.load(Relaxed),
            dropped_full: s.counters.dropped_full.load(Relaxed),
            timer_overruns: s.counters.timer_overruns.load(Relaxed),
            failed_stack_reads: s.counters.failed_stack_reads.load(Relaxed),
            truncated_stacks: s.counters.truncated_stacks.load(Relaxed),
            registration_failures: s.counters.registration_failures.load(Relaxed),
            reported_samples: s.counters.reported_samples.load(Relaxed),
            dropped_processing: s.counters.dropped_processing.load(Relaxed),
            dropped_upload: s.counters.dropped_upload.load(Relaxed),
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
        fn file_symbolization_progresses_while_dynamic_loader_is_locked() {
            struct Probe {
                start: std::sync::mpsc::Sender<()>,
                done: std::sync::mpsc::Receiver<bool>,
                passed: bool,
            }
            unsafe extern "C" fn locked(
                _: *mut libc::dl_phdr_info,
                _: usize,
                data: *mut c_void,
            ) -> i32 {
                // glibc invokes this callback while holding its loader lock.
                // A timeout releases it, so a regression fails instead of
                // leaving the test runner deadlocked.
                let probe = unsafe { &mut *data.cast::<Probe>() };
                let _ = probe.start.send(());
                probe.passed = probe
                    .done
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap_or(false);
                1
            }
            let (start, ready) = std::sync::mpsc::channel();
            let (finished, done) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready.recv().unwrap();
                let pc = resolve_file as *const () as usize;
                let mut symbols = Symbols::default();
                symbols.refresh(&[pc]);
                let valid = symbols.frames.get(&pc).is_some_and(|frames| {
                    frames.iter().any(|f| {
                        f.name.as_ref().is_some_and(|n| n.contains("resolve_file"))
                            && f.filename
                                .as_ref()
                                .is_some_and(|n| n.ends_with("profiles.rs"))
                            && f.line.is_some_and(|line| line > 0)
                    })
                });
                let old = symbols.frames.get(&pc).map(Vec::as_ptr);
                let mappings = symbols.mappings.clone();
                symbols.refresh(&[pc]);
                // Other tests may load GStreamer plugins concurrently. Reuse
                // cached frames only when the executable mappings stayed put.
                let _ = finished.send(
                    valid
                        && (mappings != symbols.mappings
                            || old == symbols.frames.get(&pc).map(Vec::as_ptr)),
                );
            });
            let mut probe = Probe {
                start,
                done,
                passed: false,
            };
            // SAFETY: callback borrows probe only for the duration of this call.
            unsafe {
                libc::dl_iterate_phdr(Some(locked), (&mut probe as *mut Probe).cast());
            }
            worker.join().unwrap();
            assert!(probe.passed, "symbolization must preserve file/line information without waiting for the loader lock");
        }

        #[test]
        fn full_or_failed_upload_queue_never_blocks_processing() {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let counters = Counters::default();
            let session = || Session {
                config: PyroscopeConfig::new("http://localhost", "test", 100, "test", "test"),
                batch: ReportBatch {
                    profile_type: "process_cpu".into(),
                    data: ReportData::RawPprof(vec![]),
                },
                from: 0,
                until: 0,
            };
            enqueue(&tx, session(), 3, &counters).unwrap();
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                enqueue(&tx, session(), 5, &counters).unwrap();
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            assert_eq!(counters.reported_samples.load(Relaxed), 3);
            assert_eq!(counters.dropped_upload.load(Relaxed), 5000);
            drop(rx);
            assert_eq!(
                enqueue(&tx, session(), 7, &counters),
                Err(FlushError::ExportFailed)
            );
            assert_eq!(counters.dropped_upload.load(Relaxed), 5007);
        }

        #[test]
        fn executable_map_parser_preserves_paths_and_offsets() {
            let map =
                Mapping::parse("00401000-00405000 r-xp 00001000 08:01 123 /a path/with  spaces.so")
                    .unwrap();
            assert_eq!(
                (map.start, map.end, map.offset, map.inode),
                (0x401000, 0x405000, 0x1000, 123)
            );
            assert_eq!(map.path, PathBuf::from("/a path/with  spaces.so"));
            assert!(Mapping::parse(
                "00401000-00405000 r-xp 00001000 08:01 123 /removed.so (deleted)"
            )
            .is_none());
            assert!(Mapping::parse("00401000-00405000 rw-p 00001000 08:01 123 /data.so").is_none());
        }

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
                for (key, _) in std::env::vars()
                    .filter(|(k, _)| k.starts_with("GST_") || k.starts_with("OTEL_"))
                {
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
            initialize(&opentelemetry_sdk::Resource::builder_empty().build()).unwrap();
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
            assert_eq!(&sample.pcs[1..sample.len], &[1999, 2999]);
            for bad in [999, 1001, usize::MAX] {
                assert!(unwind(&mut Sample::default(), &stack, 1000, bad));
            }
            stack[..8].copy_from_slice(&1000usize.to_ne_bytes());
            assert!(unwind(&mut Sample::default(), &stack, 1000, 1000));
        }
        #[test]
        fn aggregation_and_pprof_keep_identical_stacks_with_different_span_ids_separate() {
            let sampler = Arc::new(Sampler::new(100, libc::SIGRTMIN() + 6));
            sampler.slots[0].prepare();
            for span in [11, 11, 12] {
                assert!(sampler.slots[0].push(Sample {
                    ids: [1, 2, span],
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
