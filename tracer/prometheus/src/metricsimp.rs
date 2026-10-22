use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    env,
    os::raw::c_void,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex, OnceLock,
    },
    thread,
    time::Duration,
};

use glib::{
    ffi::{gboolean, GTRUE},
    translate::{FromGlibPtrBorrow, IntoGlib, ToGlibPtr},
    Quark,
};
use gst::{ffi, prelude::*};
use gstreamer as gst;
use prometheus::{
    gather, register_gauge_vec, register_histogram_vec, register_int_counter_vec,
    register_int_gauge_vec, Encoder, Gauge, GaugeVec, Histogram, HistogramVec, IntCounter,
    IntCounterVec, IntGauge, IntGaugeVec, TextEncoder,
};
use tiny_http::{Header, Response, Server};

use crate::metric_lifecycle;

// Legacy names remain for dashboard compatibility. Values are exclusive
// synchronous gst_pad_push wall time in nanoseconds.
static LATENCY_LAST: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "gst_element_latency_last_gauge",
        "Last exclusive synchronous gst_pad_push wall time in nanoseconds",
        &["element", "src_pad", "sink_pad", "path"]
    )
    .unwrap()
});
static LATENCY_SUM: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_latency_sum_count",
        "Sum of exclusive synchronous gst_pad_push wall time in nanoseconds",
        &["element", "src_pad", "sink_pad", "path"]
    )
    .unwrap()
});
static LATENCY_COUNT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_latency_count_count",
        "Count of synchronous gst_pad_push wall-time measurements",
        &["element", "src_pad", "sink_pad", "path"]
    )
    .unwrap()
});
static SYNC_WALL_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_element_sync_wall_seconds",
        "Sampled exclusive synchronous gst_pad_push wall time in seconds",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});
static SYNC_CPU_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_element_sync_cpu_seconds",
        "Sampled exclusive thread CPU time spent inside synchronous gst_pad_push calls",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});
static SYNC_BLOCKED_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_element_sync_blocked_seconds",
        "Sampled exclusive synchronous wall time not accounted for by thread CPU time",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});
static BUFFER_PTS_AGE_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_buffer_pts_age_seconds",
        "Sampled pipeline running-time minus buffer PTS running-time at an element input",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});
static IO_COUNTS_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    env::var("GST_PROM_LATENCY_IO_COUNTS")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
});
static BUFFERS_IN: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_buffers_in_total",
        "Buffers offered to real sink pads by pushes, or returned by successful pulls",
        &["element", "pad", "path"]
    )
    .unwrap()
});
static BUFFERS_OUT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_buffers_out_total",
        "Buffers submitted by real source pads in pushes, or returned by successful pulls",
        &["element", "pad", "path"]
    )
    .unwrap()
});
static PULL_LAST: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "gst_element_pull_last_nanoseconds",
        "Last exclusive synchronous pull wall time in nanoseconds",
        &["element", "src_pad", "sink_pad", "path"]
    )
    .unwrap()
});
static PULL_SUM: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_pull_wall_nanoseconds_total",
        "Sum of exclusive synchronous pull wall time in nanoseconds",
        &["element", "src_pad", "sink_pad", "path"]
    )
    .unwrap()
});
static PULL_COUNT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_pull_calls_total",
        "Completed synchronous pull calls, including EOS and errors",
        &["element", "src_pad", "sink_pad", "path"]
    )
    .unwrap()
});
static PULL_WALL: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_element_pull_wall_seconds",
        "Sampled exclusive synchronous pull wall time",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});
static PULL_CPU: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_element_pull_cpu_seconds",
        "Sampled exclusive streaming-thread CPU time inside pull calls",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});
static PULL_BLOCKED: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "gst_element_pull_blocked_seconds",
        "Sampled exclusive pull wall time not accounted for by thread CPU time",
        &["element", "src_pad", "sink_pad", "path"],
        latency_buckets()
    )
    .unwrap()
});

static HISTOGRAM_SAMPLE_RATE: LazyLock<u64> = LazyLock::new(|| {
    env::var("GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(16)
});
static ELEMENT_INCLUDE_PATTERNS: LazyLock<Vec<String>> = LazyLock::new(|| {
    env::var("GST_PROM_LATENCY_INCLUDE")
        .unwrap_or_else(|_| "*".to_string())
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
});
static PTS_AGE_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    env::var("GST_PROM_LATENCY_PTS_AGE")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
});

struct ThreadState {
    spans: RefCell<Vec<SpanFrame>>,
    // Bounded direct-mapped cache. Entries own metric snapshots, never pads.
    snapshots: RefCell<[PadSnapshot; 64]>,
    // Reuse snapshots on direct-map collisions without repeated handle copies.
    // Only consulted on misses, and cleared when it reaches 256 entries.
    cache_pool: RefCell<HashMap<usize, Rc<PadCacheData>>>,
    // Independent of cache eviction; hashing breaks fixed pipeline periods.
    sample_sequence: Cell<u64>,
    sample_rate: u64,
}

thread_local! {
    static THREAD_STATE: ThreadState = ThreadState {
        spans: RefCell::new(Vec::new()),
        snapshots: RefCell::new([const { PadSnapshot { pad: 0, cache: None } }; 64]),
        cache_pool: RefCell::new(HashMap::new()),
        sample_sequence: Cell::new(0),
        sample_rate: *HISTOGRAM_SAMPLE_RATE,
    };
}

static QUEUE_LEVEL_BUFFERS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "gst_queue_current_level_buffers",
        "Current number of buffers held by a GStreamer queue",
        &["element", "path"]
    )
    .unwrap()
});
static QUEUE_LEVEL_BYTES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "gst_queue_current_level_bytes",
        "Current bytes held by a GStreamer queue",
        &["element", "path"]
    )
    .unwrap()
});
static QUEUE_LEVEL_TIME: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "gst_queue_current_level_time_seconds",
        "Current media duration held by a GStreamer queue in seconds",
        &["element", "path"]
    )
    .unwrap()
});
static QUEUE_OVERRUNS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_queue_overruns_total",
        "Number of GStreamer queue overrun signals",
        &["element", "path"]
    )
    .unwrap()
});
static QUEUE_UNDERRUNS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_queue_underruns_total",
        "Number of GStreamer queue underrun signals",
        &["element", "path"]
    )
    .unwrap()
});
static QUEUE_REGISTRATIONS: LazyLock<Mutex<Vec<QueueRegistration>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static QUEUE_SAMPLER_ONCE: OnceLock<()> = OnceLock::new();
static QUEUE_SAMPLE_INTERVAL_MS: LazyLock<u64> = LazyLock::new(|| {
    env::var("GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(100)
});

fn latency_buckets() -> Vec<f64> {
    vec![
        0.000_001,
        0.000_002_5,
        0.000_005,
        0.000_01,
        0.000_025,
        0.000_05,
        0.000_1,
        0.000_25,
        0.000_5,
        0.001,
        0.0025,
        0.005,
        0.01,
        0.025,
        0.05,
        0.075,
        0.1,
        0.25,
        0.5,
        0.75,
        1.0,
        2.5,
        5.0,
        10.0,
        15.0,
        20.0,
        30.0,
        60.0,
        240.0,
    ]
}

struct SpanFrame {
    cache: Option<Rc<PadCacheData>>,
    pad: *mut gst::ffi::GstPad,
    start_wall_ns: u64,
    start_cpu_ns: Option<u64>,
    child_wall_ns: u64,
    child_cpu_ns: u64,
    child_cpu_valid: bool,
    sampled: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct CompletedSpan {
    exclusive_wall_ns: u64,
    exclusive_cpu_ns: Option<u64>,
    sampled: bool,
}

struct QueueRegistration {
    element: glib::WeakRef<gst::Element>,
    metrics: Option<QueueMetrics>,
    overruns: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
}

struct QueueMetrics {
    labels: (String, String),
    generation: u64,
    level_buffers: IntGauge,
    level_bytes: IntGauge,
    level_time: Gauge,
    overruns: IntCounter,
    underruns: IntCounter,
}

static PAD_CACHE_QUARK: LazyLock<glib::ffi::GQuark> =
    LazyLock::new(|| Quark::from_str("mars-metrics.pad-cache").into_glib());

static METRICS_SERVER_ONCE: OnceLock<()> = OnceLock::new();
pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "mars-metrics",
        gst::DebugColorFlags::empty(),
        Some("Prometheus tracer"),
    )
});

// Changes to links, parenting or activation invalidate labels. Streaming only
// reads this atomic; graph traversal and metric lookup happen on cache misses.
static TOPOLOGY_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Data structure to hold cached pad information used for latency measurement.
#[derive(Clone)]
struct PadCacheData {
    /// Pointer to the peer pad, used during unlink to verify the pad pair.
    peer: usize,
    generation: u64,

    metrics: Option<PadMetrics>,
    io: Option<Box<IoMetrics>>,
}

// Each thread owns a copy of the metric handles. Rc retains that snapshot
// across each call without atomic reference-count operations. GLib qdata still
// uses Arc for cross-thread access and replacement.
struct PadSnapshot {
    pad: usize,
    cache: Option<Rc<PadCacheData>>,
}

#[derive(Clone)]
struct IoMetrics {
    incoming: Option<IntCounter>,
    outgoing: Option<IntCounter>,
}

impl IoMetrics {
    fn observe(&self, buffers: u64) {
        if let Some(counter) = &self.incoming {
            counter.inc_by(buffers);
        }
        if let Some(counter) = &self.outgoing {
            counter.inc_by(buffers);
        }
    }
}

#[derive(Clone)]
struct PadMetrics {
    last_gauge: IntGauge,
    sum_counter: IntCounter,
    count_counter: IntCounter,
    wall_histogram: Histogram,
    cpu_histogram: Histogram,
    blocked_histogram: Histogram,
    pts_age_histogram: Option<Histogram>,
    sink_element: glib::WeakRef<gst::Element>,
}

#[derive(Default)]
pub struct MetricsTracerImp;

impl MetricsTracerImp {
    /// Register all tracing hooks on construction
    pub fn constructed(&self, tracer_obj: &gst::Tracer) {
        // Hook callbacks
        unsafe extern "C" fn do_push_buffer_pre<const IO: bool>(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            buf_ptr: *mut gst::ffi::GstBuffer,
        ) {
            MetricsTracerImp::do_send_latency_ts::<false, IO>(ts, pad, buf_ptr, 1);
        }

        unsafe extern "C" fn do_push_buffer_post<const IO: bool>(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            _result: gst::ffi::GstFlowReturn,
        ) {
            MetricsTracerImp::do_receive_and_record_latency_ts::<false, IO>(ts, pad, false);
        }

        unsafe extern "C" fn do_push_list_pre<const IO: bool>(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            list_ptr: *mut gst::ffi::GstBufferList,
        ) {
            let buffers = if IO {
                u64::from(ffi::gst_buffer_list_length(list_ptr))
            } else {
                0
            };
            MetricsTracerImp::do_send_latency_ts::<false, IO>(
                ts,
                pad,
                std::ptr::null_mut(),
                buffers,
            );
        }

        unsafe extern "C" fn do_push_list_post<const IO: bool>(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            _result: gst::ffi::GstFlowReturn,
        ) {
            MetricsTracerImp::do_receive_and_record_latency_ts::<false, IO>(ts, pad, false);
        }

        unsafe extern "C" fn do_pull_range_pre<const IO: bool>(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            _offset: u64,
            _size: libc::c_uint,
        ) {
            MetricsTracerImp::do_send_latency_ts::<true, IO>(ts, pad, std::ptr::null_mut(), 0);
        }
        unsafe extern "C" fn do_pull_range_post<const IO: bool>(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            buffer: *mut gst::ffi::GstBuffer,
            result: gst::ffi::GstFlowReturn,
        ) {
            MetricsTracerImp::do_receive_and_record_latency_ts::<true, IO>(
                ts,
                pad,
                result >= ffi::GST_FLOW_OK && !buffer.is_null(),
            );
        }

        unsafe extern "C" fn do_pad_link_post(
            _tracer: *mut gst::Tracer,
            _ts: u64,
            src_pad: *mut gst::ffi::GstPad,
            _sink_pad: *mut gst::ffi::GstPad,
            res: gst::ffi::GstPadLinkReturn,
        ) {
            if res == ffi::GST_PAD_LINK_OK {
                MetricsTracerImp::invalidate_topology();
                // Defer label creation until data flows, after graph assembly.
                glib::gobject_ffi::g_object_set_qdata_full(
                    src_pad.cast(),
                    *PAD_CACHE_QUARK,
                    std::ptr::null_mut(),
                    None,
                );
            }
        }

        unsafe extern "C" fn do_pad_unlink_post(
            _tracer: *mut gst::Tracer,
            _ts: u64,
            src_pad: *mut gst::ffi::GstPad,
            sink_pad: *mut gst::ffi::GstPad,
            res: gboolean,
        ) {
            if res == GTRUE {
                MetricsTracerImp::invalidate_topology();
                if let Some(cache) = MetricsTracerImp::pad_cache(src_pad) {
                    if cache.peer == sink_pad as usize {
                        // A concurrent relink may already have installed a new
                        // cache. Only remove the exact snapshot we inspected.
                        MetricsTracerImp::replace_pad_cache(src_pad, Some(&cache), None);
                    }
                }
            }
        }

        unsafe {
            // Push hooks; majority of the time we are pushing.
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-pre".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    if *IO_COUNTS_ENABLED {
                        do_push_buffer_pre::<true> as *const ()
                    } else {
                        do_push_buffer_pre::<false> as *const ()
                    },
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    if *IO_COUNTS_ENABLED {
                        do_push_buffer_post::<true> as *const ()
                    } else {
                        do_push_buffer_post::<false> as *const ()
                    },
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-list-pre".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    if *IO_COUNTS_ENABLED {
                        do_push_list_pre::<true> as *const ()
                    } else {
                        do_push_list_pre::<false> as *const ()
                    },
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-list-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    if *IO_COUNTS_ENABLED {
                        do_push_list_post::<true> as *const ()
                    } else {
                        do_push_list_post::<false> as *const ()
                    },
                ),
            );
            // Pull hooks; far less common, but still useful.
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-pull-range-pre".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    if *IO_COUNTS_ENABLED {
                        do_pull_range_pre::<true> as *const ()
                    } else {
                        do_pull_range_pre::<false> as *const ()
                    },
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-pull-range-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    if *IO_COUNTS_ENABLED {
                        do_pull_range_post::<true> as *const ()
                    } else {
                        do_pull_range_post::<false> as *const ()
                    },
                ),
            );
            // Link hooks; allow us to populate and clear the pads' quark cache.
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-link-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_pad_link_post as *const (),
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-unlink-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_pad_unlink_post as *const (),
                ),
            );
        }
    }

    /// Handle the element-new hook
    pub fn element_new(&self, _ts: u64, element: &gst::Element, port: u16) {
        if element.is::<gst::Pipeline>() {
            // Watching every element disables GLib's no-handler signal fast
            // path (notably identity's per-buffer handoff). Only root pipeline
            // names can change while parented elements are streaming.
            element.connect_notify(Some("name"), |_, _| Self::invalidate_topology());
        }
        if element.is::<gst::Pipeline>() && port > 0 {
            METRICS_SERVER_ONCE.get_or_init(|| Self::maybe_start_metrics_server(port));
        }

        if Self::is_queue(element) {
            Self::register_queue(element);
            QUEUE_SAMPLER_ONCE.get_or_init(Self::start_queue_sampler);
        }
    }

    // Add this function, which is the handler for the "metrics" signal
    pub fn request_metrics() -> String {
        metric_lifecycle::prune();
        let metric_families = gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("Failed to encode metrics");

        String::from_utf8(buffer).expect("Metrics buffer is not valid UTF-8")
    }

    /// Resolve proxy pads using owned wrappers; each temporary reference drops.
    fn resolve_pad(pad: *mut ffi::GstPad) -> Option<gst::Pad> {
        // from_glib_none sinks floating GObjects. Dynamic RTSP pads can still
        // be floating here: consuming that reference would free the caller's
        // pad when our wrapper drops. Borrow, then clone with g_object_ref.
        let borrowed = unsafe { gst::Pad::from_glib_borrow(pad) };
        let mut pad = (*borrowed).clone();
        // A malformed or changing topology must not recurse indefinitely.
        for _ in 0..64 {
            if let Some(ghost) = pad.downcast_ref::<gst::GhostPad>() {
                pad = ghost.target()?;
            } else if pad.is::<gst::ProxyPad>() {
                pad = pad.parent()?.downcast::<gst::Pad>().ok()?.peer()?;
            } else {
                return Some(pad);
            }
        }
        None
    }

    /// Release qdata's ownership. In-flight spans retain their own reference.
    unsafe extern "C" fn drop_pad_cache(ptr: *mut c_void) {
        drop(Arc::from_raw(ptr.cast::<PadCacheData>()));
    }

    /// GLib holds the qdata lock while duplicating, so unlink cannot free the
    /// cache between reading its pointer and acquiring our reference.
    unsafe fn pad_cache(pad: *mut ffi::GstPad) -> Option<Arc<PadCacheData>> {
        unsafe extern "C" fn retain(ptr: *mut c_void, _: *mut c_void) -> *mut c_void {
            if !ptr.is_null() {
                Arc::increment_strong_count(ptr.cast::<PadCacheData>());
            }
            ptr
        }
        let ptr = glib::gobject_ffi::g_object_dup_qdata(
            pad.cast(),
            *PAD_CACHE_QUARK,
            Some(retain),
            std::ptr::null_mut(),
        );
        (!ptr.is_null()).then(|| Arc::from_raw(ptr.cast::<PadCacheData>()))
    }

    pub(crate) fn invalidate_topology() {
        TOPOLOGY_GENERATION.fetch_add(1, Ordering::Release);
    }

    /// Atomically replace qdata while retaining in-flight snapshots. On success
    /// GLib transfers the old qdata reference to us; on failure we reclaim the
    /// new reference that GLib did not take. No borrowed pointer escapes.
    unsafe fn replace_pad_cache(
        pad: *mut ffi::GstPad,
        previous: Option<&Arc<PadCacheData>>,
        next: Option<&Arc<PadCacheData>>,
    ) -> bool {
        let old = previous.map_or(std::ptr::null_mut(), |cache| {
            Arc::as_ptr(cache).cast_mut().cast()
        });
        let new = next.map_or(std::ptr::null_mut(), |cache| {
            Arc::into_raw(cache.clone()).cast_mut().cast()
        });
        let destroy = next.map(|_| Self::drop_pad_cache as unsafe extern "C" fn(*mut c_void));
        let mut old_destroy = None;
        let replaced = glib::gobject_ffi::g_object_replace_qdata(
            pad.cast(),
            *PAD_CACHE_QUARK,
            old,
            new,
            destroy,
            &mut old_destroy,
        ) == GTRUE;
        if replaced {
            if let Some(destroy) = old_destroy {
                destroy(old);
            }
        } else if !new.is_null() {
            Self::drop_pad_cache(new);
        }
        replaced
    }

    // Keep the cache hit inline so the caller can reuse its TLS access.
    #[inline(always)]
    unsafe fn current_pad_cache(state: &ThreadState, pad: *mut ffi::GstPad) -> Rc<PadCacheData> {
        let generation = TOPOLOGY_GENERATION.load(Ordering::Acquire);
        let key = pad as usize;
        let index = ((key >> 4) ^ (key >> 12)) & 63;
        let cached = {
            let slots = state.snapshots.borrow();
            let slot = &slots[index];
            slot.cache
                .as_ref()
                .filter(|cache| slot.pad == key && cache.generation == generation)
                .cloned()
        };
        if let Some(cache) = cached {
            return cache;
        }
        // Drop the TLS borrow before GObject calls, which can reenter tracing.
        let cache = Self::thread_cache_miss(state, pad, generation);
        state.snapshots.borrow_mut()[index] = PadSnapshot {
            pad: key,
            cache: Some(cache.clone()),
        };
        cache
    }

    #[cold]
    unsafe fn thread_cache_miss(
        state: &ThreadState,
        pad: *mut ffi::GstPad,
        generation: u64,
    ) -> Rc<PadCacheData> {
        let key = pad as usize;
        let cached = state
            .cache_pool
            .borrow()
            .get(&key)
            .filter(|cache| cache.generation == generation)
            .cloned();
        if let Some(cache) = cached {
            return cache;
        }
        // GObject calls may reenter tracing; no TLS borrow is held here.
        let cache = Rc::new(Self::refresh_pad_cache(pad).as_ref().clone());
        let mut pool = state.cache_pool.borrow_mut();
        if pool.len() >= 256 {
            pool.clear();
        }
        pool.insert(key, cache.clone());
        cache
    }

    #[cold]
    unsafe fn refresh_pad_cache(pad: *mut ffi::GstPad) -> Arc<PadCacheData> {
        loop {
            let generation = TOPOLOGY_GENERATION.load(Ordering::Acquire);
            let previous = Self::pad_cache(pad);
            if let Some(cache) = &previous {
                if cache.generation == generation {
                    return previous.unwrap();
                }
            }
            let borrowed = gst::Pad::from_glib_borrow(pad);
            let peer = borrowed.peer();
            let cache = Self::create_pad_cache(&borrowed, peer.as_ref(), generation);
            if Self::replace_pad_cache(pad, previous.as_ref(), Some(&cache)) {
                return cache;
            }
            // Another streaming thread or a topology hook won the race. Reuse
            // its snapshot instead of overwriting a newer link's cache.
        }
    }

    #[cfg(test)]
    fn do_create_latency_cache_for_pad_pair(
        pad: *mut ffi::GstPad,
        peer: *mut ffi::GstPad,
        generation: u64,
    ) -> Option<Arc<PadCacheData>> {
        if pad.is_null() || peer.is_null() {
            return None;
        }
        let pad = unsafe { gst::Pad::from_glib_borrow(pad) };
        let peer = unsafe { gst::Pad::from_glib_borrow(peer) };
        Some(Self::create_pad_cache(&pad, Some(&peer), generation))
    }

    fn create_pad_cache(
        pad: &gst::Pad,
        peer: Option<&gst::Pad>,
        generation: u64,
    ) -> Arc<PadCacheData> {
        // Pull hooks arrive on sink pads; push hooks arrive on source pads.
        let pull = pad.direction() == gst::PadDirection::Sink;
        let (source, sink) = if pull {
            (peer, Some(pad))
        } else {
            (Some(pad), peer)
        };
        let io = (*IO_COUNTS_ENABLED)
            .then(|| IoMetrics {
                incoming: sink.and_then(|pad| Self::io_counter(pad, true)),
                outgoing: source.and_then(|pad| Self::io_counter(pad, false)),
            })
            .filter(|io| io.incoming.is_some() || io.outgoing.is_some())
            .map(Box::new);
        let metrics = peer.and_then(|peer| {
            // Count forwarding only at the real callee. Intermediate ghost/proxy
            // calls remain stack frames so their children are subtracted.
            if peer.is::<gst::ProxyPad>() {
                return None;
            }
            let caller = Self::resolve_pad(pad.as_ptr())?;
            let element = peer.parent_element()?;
            if !Self::element_selected(element.name().as_str()) {
                return None;
            }
            let (source, sink) = if pull {
                (peer, &caller)
            } else {
                (&caller, peer)
            };
            let element_name = element.name();
            let src_name = source.name();
            let sink_name = sink.name();
            let path = element
                .parent()
                .map(|parent| parent.path_string().to_string())
                .unwrap_or_else(|| "none".to_owned());
            let labels: [&str; 4] = [&element_name, &src_name, &sink_name, &path];
            let remove = if pull {
                Self::remove_pull_series
            } else {
                Self::remove_push_series
            };
            Some(metric_lifecycle::with_series(
                if pull { "pull" } else { "push" },
                &labels,
                &[source.upcast_ref(), sink.upcast_ref()],
                remove,
                || PadMetrics {
                    last_gauge: if pull {
                        PULL_LAST.with_label_values(&labels)
                    } else {
                        LATENCY_LAST.with_label_values(&labels)
                    },
                    sum_counter: if pull {
                        PULL_SUM.with_label_values(&labels)
                    } else {
                        LATENCY_SUM.with_label_values(&labels)
                    },
                    count_counter: if pull {
                        PULL_COUNT.with_label_values(&labels)
                    } else {
                        LATENCY_COUNT.with_label_values(&labels)
                    },
                    wall_histogram: if pull {
                        PULL_WALL.with_label_values(&labels)
                    } else {
                        SYNC_WALL_HISTOGRAM.with_label_values(&labels)
                    },
                    cpu_histogram: if pull {
                        PULL_CPU.with_label_values(&labels)
                    } else {
                        SYNC_CPU_HISTOGRAM.with_label_values(&labels)
                    },
                    blocked_histogram: if pull {
                        PULL_BLOCKED.with_label_values(&labels)
                    } else {
                        SYNC_BLOCKED_HISTOGRAM.with_label_values(&labels)
                    },
                    pts_age_histogram: (!pull && *PTS_AGE_ENABLED)
                        .then(|| BUFFER_PTS_AGE_HISTOGRAM.with_label_values(&labels)),
                    sink_element: element.downgrade(),
                },
            ))
        });
        Arc::new(PadCacheData {
            peer: peer.map_or(0, |peer| peer.as_ptr() as usize),
            generation,
            metrics,
            io,
        })
    }

    fn io_counter(pad: &gst::Pad, incoming: bool) -> Option<IntCounter> {
        if pad.is::<gst::ProxyPad>() {
            return None;
        }
        let element = pad.parent_element()?;
        let name = element.name();
        if !Self::element_selected(&name) {
            return None;
        }
        // Initialize every real pad on the selected element, including zero
        // outputs on dropping/buffering elements. This only runs on cache misses.
        let counter = Self::io_counter_for_pad(pad, &element, incoming);
        for other in element.pads() {
            if other != *pad && !other.is::<gst::ProxyPad>() {
                Self::io_counter_for_pad(
                    &other,
                    &element,
                    other.direction() == gst::PadDirection::Sink,
                );
            }
        }
        Some(counter)
    }

    fn io_counter_for_pad(pad: &gst::Pad, element: &gst::Element, incoming: bool) -> IntCounter {
        let name = element.name();
        let pad_name = pad.name();
        let path = element
            .parent()
            .map(|parent| parent.path_string().to_string())
            .unwrap_or_else(|| "none".to_owned());
        let labels: [&str; 3] = [&name, &pad_name, &path];
        let (family, metric, remove): (_, _, fn(&[&str])) = if incoming {
            ("in", &*BUFFERS_IN, Self::remove_in_series)
        } else {
            ("out", &*BUFFERS_OUT, Self::remove_out_series)
        };
        metric_lifecycle::with_series(family, &labels, &[pad.upcast_ref()], remove, || {
            metric.with_label_values(&labels)
        })
    }

    fn remove_in_series(labels: &[&str]) {
        let _ = BUFFERS_IN.remove_label_values(labels);
    }
    fn remove_out_series(labels: &[&str]) {
        let _ = BUFFERS_OUT.remove_label_values(labels);
    }
    fn remove_pull_series(labels: &[&str]) {
        let _ = PULL_LAST.remove_label_values(labels);
        let _ = PULL_SUM.remove_label_values(labels);
        let _ = PULL_COUNT.remove_label_values(labels);
        let _ = PULL_WALL.remove_label_values(labels);
        let _ = PULL_CPU.remove_label_values(labels);
        let _ = PULL_BLOCKED.remove_label_values(labels);
    }

    fn remove_push_series(labels: &[&str]) {
        let _ = LATENCY_LAST.remove_label_values(labels);
        let _ = LATENCY_SUM.remove_label_values(labels);
        let _ = LATENCY_COUNT.remove_label_values(labels);
        let _ = SYNC_WALL_HISTOGRAM.remove_label_values(labels);
        let _ = SYNC_CPU_HISTOGRAM.remove_label_values(labels);
        let _ = SYNC_BLOCKED_HISTOGRAM.remove_label_values(labels);
        if *PTS_AGE_ENABLED {
            let _ = BUFFER_PTS_AGE_HISTOGRAM.remove_label_values(labels);
        }
    }

    fn remove_queue_series(labels: &[&str]) {
        let _ = QUEUE_LEVEL_BUFFERS.remove_label_values(labels);
        let _ = QUEUE_LEVEL_BYTES.remove_label_values(labels);
        let _ = QUEUE_LEVEL_TIME.remove_label_values(labels);
        let _ = QUEUE_OVERRUNS.remove_label_values(labels);
        let _ = QUEUE_UNDERRUNS.remove_label_values(labels);
    }

    #[cfg(test)]
    fn pad_name(pad: *mut ffi::GstPad) -> String {
        unsafe { gst::Pad::from_glib_borrow(pad).name().to_string() }
    }

    unsafe fn do_send_latency_ts<const PULL: bool, const IO: bool>(
        _ts: u64,
        src_pad: *mut gst::ffi::GstPad,
        buffer: *mut gst::ffi::GstBuffer,
        buffers: u64,
    ) {
        THREAD_STATE.with(|state| {
            let cache = Some(Self::current_pad_cache(state, src_pad));
            if IO && !PULL {
                if let Some(io) = cache.as_ref().and_then(|cache| cache.io.as_ref()) {
                    io.observe(buffers);
                }
            }
            let sampled = cache.as_ref().is_some_and(|cache| {
                if cache.metrics.is_none() {
                    return false;
                }
                let sequence = &state.sample_sequence;
                let value = sequence.get();
                sequence.set(value.wrapping_add(1));
                let seed = sequence.as_ptr() as usize ^ src_pad as usize;
                Self::sample_sequence(value, seed as u64, state.sample_rate)
            });
            // Perform optional diagnostics before starting the span, and outside
            // the mutable TLS borrow (GObject calls may invoke other callbacks).
            if sampled && !buffer.is_null() {
                if let Some(metrics) = cache.as_ref().and_then(|cache| cache.metrics.as_ref()) {
                    if let Some(histogram) = &metrics.pts_age_histogram {
                        if let Some(age) = Self::buffer_pts_age(src_pad, buffer, metrics) {
                            histogram.observe(age);
                        }
                    }
                }
            }
            let mut stack = state.spans.borrow_mut();
            let selected = cache.as_ref().is_some_and(|cache| cache.metrics.is_some());
            let needs_frame =
                selected || (IO && PULL && cache.as_ref().is_some_and(|cache| cache.io.is_some()));
            if !needs_frame && stack.is_empty() {
                return;
            }
            // A sampled parent needs each direct child's inclusive CPU time.
            // That requirement does not extend to unsampled grandchildren.
            let measure_cpu = sampled || stack.last().is_some_and(|frame| frame.sampled);
            let start_cpu_ns = measure_cpu.then(Self::thread_cpu_time_ns).flatten();
            stack.push(SpanFrame {
                cache,
                pad: src_pad,
                start_wall_ns: gst::get_timestamp().nseconds(),
                start_cpu_ns,
                child_wall_ns: 0,
                child_cpu_ns: 0,
                child_cpu_valid: true,
                sampled,
            });
        });
    }

    unsafe fn do_receive_and_record_latency_ts<const PULL: bool, const IO: bool>(
        _ts: u64,
        src_pad: *mut gst::ffi::GstPad,
        returned_buffer: bool,
    ) {
        // Use the same clock at both boundaries, excluding this span's metric
        // export work. Nested tracer bookkeeping still adds measurement noise.
        let wall_end_ns = gst::get_timestamp().nseconds();
        let completed = THREAD_STATE.with(|state| {
            let mut stack = state.spans.borrow_mut();
            let measure_cpu = stack.last()?.start_cpu_ns.is_some();
            let cpu_end_ns = measure_cpu.then(Self::thread_cpu_time_ns).flatten();
            Self::complete_span(&mut stack, src_pad, wall_end_ns, cpu_end_ns)
        });
        let Some((completed, cache)) = completed else {
            return;
        };
        if IO && PULL && returned_buffer {
            if let Some(io) = cache.as_ref().and_then(|cache| cache.io.as_ref()) {
                io.observe(1);
            }
        }
        let Some(metrics) = cache.as_ref().and_then(|cache| cache.metrics.as_ref()) else {
            return;
        };
        metrics
            .last_gauge
            .set(completed.exclusive_wall_ns.try_into().unwrap_or(i64::MAX));
        metrics.sum_counter.inc_by(completed.exclusive_wall_ns);
        metrics.count_counter.inc();
        if completed.sampled {
            metrics
                .wall_histogram
                .observe(completed.exclusive_wall_ns as f64 / 1_000_000_000f64);
            if let Some(cpu_ns) = completed.exclusive_cpu_ns {
                metrics
                    .cpu_histogram
                    .observe(cpu_ns as f64 / 1_000_000_000f64);
                metrics.blocked_histogram.observe(
                    completed.exclusive_wall_ns.saturating_sub(cpu_ns) as f64 / 1_000_000_000f64,
                );
            }
        }
    }

    fn complete_span(
        stack: &mut Vec<SpanFrame>,
        pad: *mut gst::ffi::GstPad,
        wall_end_ns: u64,
        cpu_end_ns: Option<u64>,
    ) -> Option<(CompletedSpan, Option<Rc<PadCacheData>>)> {
        let frame = stack.pop()?;
        if frame.pad != pad {
            gst::warning!(
                CAT,
                "unbalanced timing hooks: expected {:?}, received {:?}",
                frame.pad,
                pad
            );
            stack.clear();
            return None;
        }

        let raw_wall_ns = wall_end_ns.saturating_sub(frame.start_wall_ns);
        let raw_cpu_ns = frame
            .start_cpu_ns
            .and_then(|start| cpu_end_ns.map(|end| end.saturating_sub(start)));
        if let Some(parent) = stack.last_mut() {
            parent.child_wall_ns = parent.child_wall_ns.saturating_add(raw_wall_ns);
            if parent.sampled {
                if let Some(raw_cpu_ns) = raw_cpu_ns {
                    parent.child_cpu_ns = parent.child_cpu_ns.saturating_add(raw_cpu_ns);
                } else {
                    // Invalidate exclusive CPU attribution, but preserve the
                    // parent's inclusive measurement for its own parent.
                    parent.child_cpu_valid = false;
                }
            }
        }

        Some((
            CompletedSpan {
                exclusive_wall_ns: Self::compute_element_latency(raw_wall_ns, frame.child_wall_ns),
                exclusive_cpu_ns: raw_cpu_ns
                    .filter(|_| frame.sampled && frame.child_cpu_valid)
                    .map(|cpu| Self::compute_element_latency(cpu, frame.child_cpu_ns)),
                sampled: frame.sampled,
            },
            frame.cache,
        ))
    }

    fn sample_sequence(sequence: u64, pad: u64, rate: u64) -> bool {
        if rate == 1 {
            return true;
        }
        // SplitMix64 prevents fixed batch/stream periods aliasing with sampling.
        let mut value = sequence ^ pad;
        value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        if rate.is_power_of_two() {
            value & (rate - 1) == 0
        } else {
            value.is_multiple_of(rate)
        }
    }

    fn element_selected(element: &str) -> bool {
        ELEMENT_INCLUDE_PATTERNS
            .iter()
            .any(|pattern| Self::glob_matches(pattern, element))
    }

    fn glob_matches(pattern: &str, value: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        let starts_with_wildcard = pattern.starts_with('*');
        let ends_with_wildcard = pattern.ends_with('*');
        let needle = pattern.trim_matches('*');
        match (starts_with_wildcard, ends_with_wildcard) {
            (true, true) => value.contains(needle),
            (true, false) => value.ends_with(needle),
            (false, true) => value.starts_with(needle),
            (false, false) => value == needle,
        }
    }

    fn thread_cpu_time_ns() -> Option<u64> {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) };
        (result == 0).then(|| {
            (value.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(value.tv_nsec as u64)
        })
    }

    unsafe fn buffer_pts_age(
        src_pad: *mut gst::ffi::GstPad,
        buffer: *mut gst::ffi::GstBuffer,
        metrics: &PadMetrics,
    ) -> Option<f64> {
        let buffer = gst::Buffer::from_glib_none(buffer);
        let pts = buffer.pts()?;
        let pad = gst::Pad::from_glib_borrow(src_pad);
        let segment_event = pad.sticky_event::<gst::event::Segment>(0)?;
        let segment = segment_event.segment().downcast_ref::<gst::ClockTime>()?;
        let buffer_running_time = segment.to_running_time(pts)?;
        let now = metrics.sink_element.upgrade()?.current_running_time()?;
        Some(now.saturating_sub(buffer_running_time).nseconds() as f64 / 1_000_000_000f64)
    }

    fn is_queue(element: &gst::Element) -> bool {
        // element-new runs during construction. DeepStream elements may not
        // have a usable factory pointer yet; their GType is already valid.
        matches!(element.type_().name(), "GstQueue" | "GstQueue2")
            && element.find_property("current-level-buffers").is_some()
            && element.find_property("current-level-bytes").is_some()
            && element.find_property("current-level-time").is_some()
    }

    fn register_queue(element: &gst::Element) {
        element.connect_notify(Some("name"), |_, _| Self::invalidate_topology());
        // queue2 exposes levels but has neither of queue's pressure signals.
        // Keep streaming-thread callbacks allocation-free; export on sampler.
        let overruns = Arc::new(AtomicU64::new(0));
        let underruns = Arc::new(AtomicU64::new(0));
        for (signal, counter) in [("overrun", &overruns), ("underrun", &underruns)] {
            if glib::subclass::SignalId::lookup(signal, element.type_()).is_some() {
                let counter = Arc::clone(counter);
                element.connect(signal, false, move |_| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    None
                });
            }
        }

        let element_ref = glib::WeakRef::new();
        element_ref.set(Some(element));
        QUEUE_REGISTRATIONS.lock().unwrap().push(QueueRegistration {
            element: element_ref,
            metrics: None,
            overruns,
            underruns,
        });
    }

    fn queue_labels(element: &gst::Element) -> (String, String) {
        (
            element.name().to_string(),
            element.path_string().to_string(),
        )
    }

    fn start_queue_sampler() {
        thread::spawn(|| loop {
            thread::sleep(Duration::from_millis(*QUEUE_SAMPLE_INTERVAL_MS));
            // Property reads and GObject destruction can reenter tracing. Keep
            // them outside the registration lock, preserving new registrations.
            let mut registrations = std::mem::take(&mut *QUEUE_REGISTRATIONS.lock().unwrap());
            registrations.retain_mut(|registration| {
                let Some(element) = registration.element.upgrade() else {
                    if let Some(metrics) = &registration.metrics {
                        metrics.level_buffers.set(0);
                        metrics.level_bytes.set(0);
                        metrics.level_time.set(0.0);
                    }
                    return false;
                };

                // Capture labels only after assembly, when streaming can start.
                if element.current_state() < gst::State::Paused {
                    if let Some(metrics) = &registration.metrics {
                        metrics.level_buffers.set(0);
                        metrics.level_bytes.set(0);
                        metrics.level_time.set(0.0);
                    }
                    return true;
                }

                let current_labels = Self::queue_labels(&element);
                let generation = TOPOLOGY_GENERATION.load(Ordering::Acquire);
                if registration.metrics.as_ref().is_some_and(|metrics| {
                    metrics.labels != current_labels || metrics.generation != generation
                }) {
                    registration.metrics = None;
                }
                let metrics = registration.metrics.get_or_insert_with(|| {
                    let labels = [current_labels.0.as_str(), current_labels.1.as_str()];
                    metric_lifecycle::with_series(
                        "queue",
                        &labels,
                        &[element.upcast_ref()],
                        Self::remove_queue_series,
                        || QueueMetrics {
                            labels: current_labels.clone(),
                            generation,
                            level_buffers: QUEUE_LEVEL_BUFFERS.with_label_values(&labels),
                            level_bytes: QUEUE_LEVEL_BYTES.with_label_values(&labels),
                            level_time: QUEUE_LEVEL_TIME.with_label_values(&labels),
                            overruns: QUEUE_OVERRUNS.with_label_values(&labels),
                            underruns: QUEUE_UNDERRUNS.with_label_values(&labels),
                        },
                    )
                });

                let buffers = element
                    .property_value("current-level-buffers")
                    .get::<u32>()
                    .unwrap_or_default();
                let bytes = element
                    .property_value("current-level-bytes")
                    .get::<u32>()
                    .unwrap_or_default();
                let time_ns = element
                    .property_value("current-level-time")
                    .get::<u64>()
                    .unwrap_or_default();
                metrics.level_buffers.set(i64::from(buffers));
                metrics.level_bytes.set(i64::from(bytes));
                metrics.level_time.set(time_ns as f64 / 1_000_000_000f64);
                metrics
                    .overruns
                    .inc_by(registration.overruns.swap(0, Ordering::Relaxed));
                metrics
                    .underruns
                    .inc_by(registration.underruns.swap(0, Ordering::Relaxed));
                true
            });
            QUEUE_REGISTRATIONS.lock().unwrap().extend(registrations);
        });
    }

    /// Spawn the HTTP server in a new thread on the provided port.
    fn maybe_start_metrics_server(port: u16) {
        thread::spawn(move || {
            let addr = ("0.0.0.0", port);
            let server_r = Server::http(addr);
            if server_r.is_err() {
                gst::warning!(
                    CAT,
                    "Failed to start Prometheus metrics server on 0.0.0.0:{}",
                    port
                );
                return;
            };
            let server = server_r.unwrap();

            gst::info!(CAT, "Prometheus metrics server listening on {}", port);

            for request in server.incoming_requests() {
                let buffer = Self::request_metrics().into_bytes();

                // Build and send HTTP response
                let response = Response::from_data(buffer).with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                        .unwrap(),
                );
                let _ = request.respond(response);
            }
        });
    }

    pub(crate) fn compute_element_latency(span_diff: u64, ts_latency: u64) -> u64 {
        span_diff.saturating_sub(ts_latency)
    }
}

#[cfg(test)]
mod tests {
    use super::{latency_buckets, CompletedSpan, MetricsTracerImp, SpanFrame};
    use glib::translate::{FromGlibPtrFull, ToGlibPtr};
    use gst::prelude::*;
    use gstreamer as gst;
    use std::sync::Arc;

    // These two cache tests explicitly change the process-wide generation.
    static TOPOLOGY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn empty_cache(generation: u64) -> Arc<super::PadCacheData> {
        Arc::new(super::PadCacheData {
            peer: 0,
            generation,
            metrics: None,
            io: None,
        })
    }

    #[test]
    fn colliding_pad_slots_reuse_their_thread_snapshot() {
        let _guard = TOPOLOGY_TEST_LOCK.lock().unwrap();
        gst::init().unwrap();
        let pads: Vec<_> = (0..65)
            .map(|_| gst::Pad::builder(gst::PadDirection::Src).build())
            .collect();
        let mut slots = [None; 64];
        let (first, second) = pads
            .iter()
            .enumerate()
            .find_map(|(index, pad)| {
                let key = pad.as_ptr() as usize;
                let slot = ((key >> 4) ^ (key >> 12)) & 63;
                slots[slot].replace(index).map(|first| (first, index))
            })
            .unwrap();
        super::THREAD_STATE.with(|state| unsafe {
            let original = MetricsTracerImp::current_pad_cache(state, pads[first].as_ptr());
            MetricsTracerImp::current_pad_cache(state, pads[second].as_ptr());
            let reused = MetricsTracerImp::current_pad_cache(state, pads[first].as_ptr());
            assert!(std::rc::Rc::ptr_eq(&original, &reused));
            MetricsTracerImp::invalidate_topology();
            let refreshed = MetricsTracerImp::current_pad_cache(state, pads[first].as_ptr());
            assert!(!std::rc::Rc::ptr_eq(&original, &refreshed));
        });
    }

    #[test]
    fn thread_cache_reuses_snapshots_until_topology_changes_without_retaining_elements() {
        let _guard = TOPOLOGY_TEST_LOCK.lock().unwrap();
        gst::init().unwrap();
        let source = gst::ElementFactory::make("identity")
            .name("snapshot_source")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("identity")
            .name("snapshot_sink")
            .build()
            .unwrap();
        source.link(&sink).unwrap();
        let pad = source.static_pad("src").unwrap();
        let weak_source = source.downgrade();
        let weak_sink = sink.downgrade();
        super::THREAD_STATE.with(|state| unsafe {
            let first = MetricsTracerImp::current_pad_cache(state, pad.as_ptr());
            let hit = MetricsTracerImp::current_pad_cache(state, pad.as_ptr());
            assert!(std::rc::Rc::ptr_eq(&first, &hit));
            MetricsTracerImp::invalidate_topology();
            let refreshed = MetricsTracerImp::current_pad_cache(state, pad.as_ptr());
            assert!(!std::rc::Rc::ptr_eq(&first, &refreshed));
        });
        drop(pad);
        drop(source);
        drop(sink);
        assert!(weak_source.upgrade().is_none());
        assert!(weak_sink.upgrade().is_none());
    }

    #[test]
    fn stale_unlink_cannot_remove_a_replacement_cache() {
        gst::init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        let first = empty_cache(1);
        let second = empty_cache(2);
        unsafe {
            assert!(MetricsTracerImp::replace_pad_cache(
                pad.as_ptr(),
                None,
                Some(&first)
            ));
            assert!(MetricsTracerImp::replace_pad_cache(
                pad.as_ptr(),
                Some(&first),
                Some(&second)
            ));
            assert!(!MetricsTracerImp::replace_pad_cache(
                pad.as_ptr(),
                Some(&first),
                None
            ));
            // A failed install must also release the new qdata reference.
            assert!(!MetricsTracerImp::replace_pad_cache(
                pad.as_ptr(),
                None,
                Some(&first)
            ));
            assert!(Arc::ptr_eq(
                &MetricsTracerImp::pad_cache(pad.as_ptr()).unwrap(),
                &second
            ));
        }
        drop(pad);
        assert_eq!(Arc::strong_count(&first), 1);
        assert_eq!(Arc::strong_count(&second), 1);
    }

    #[test]
    fn concurrent_cache_readers_survive_replacement_and_release_all_references() {
        gst::init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        let mut weak_caches = Vec::new();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                for _ in 0..10_000 {
                    if let Some(snapshot) = unsafe { MetricsTracerImp::pad_cache(pad.as_ptr()) } {
                        assert_eq!(snapshot.generation, 1);
                        std::hint::black_box(&snapshot);
                    }
                }
            });
            start.wait();
            for iteration in 0..10_000 {
                let cache = empty_cache(1);
                // Most allocations must actually be freed during the race so
                // ASan can detect stale accesses; retain weak probes for a
                // subset to check that ownership is released after joining.
                if iteration % 128 == 0 {
                    weak_caches.push(Arc::downgrade(&cache));
                }
                unsafe {
                    glib::gobject_ffi::g_object_set_qdata_full(
                        pad.as_ptr().cast(),
                        *super::PAD_CACHE_QUARK,
                        Arc::into_raw(cache).cast_mut().cast(),
                        Some(MetricsTracerImp::drop_pad_cache),
                    );
                    glib::gobject_ffi::g_object_set_qdata_full(
                        pad.as_ptr().cast(),
                        *super::PAD_CACHE_QUARK,
                        std::ptr::null_mut(),
                        None,
                    );
                }
            }
        });
        assert!(weak_caches.iter().all(|cache| cache.upgrade().is_none()));
    }

    #[test]
    fn given_floating_pad_when_inspected_then_callers_reference_is_untouched() {
        gst::init().unwrap();
        // Unlike the Rust builder, this keeps the C caller's floating reference.
        let pad = unsafe {
            gst::Pad::from_glib_full(gst::ffi::gst_pad_new(
                c"floating".as_ptr(),
                gst::ffi::GST_PAD_SRC,
            ))
        };
        let refs = pad.ref_count();
        let floating = || unsafe { glib::gobject_ffi::g_object_is_floating(pad.as_ptr().cast()) };
        assert_eq!(floating(), glib::ffi::GTRUE);
        assert!(MetricsTracerImp::resolve_pad(pad.to_glib_none().0)
            .unwrap()
            .parent_element()
            .is_none());
        assert_eq!(MetricsTracerImp::pad_name(pad.to_glib_none().0), "floating");
        assert_eq!(floating(), glib::ffi::GTRUE);
        assert_eq!(pad.ref_count(), refs);
    }

    #[test]
    fn given_queue_types_when_detecting_queues_then_instance_names_do_not_matter() {
        gst::init().unwrap();
        for factory in ["queue", "queue2"] {
            let element = gst::ElementFactory::make(factory)
                .name("not_a_queue_name")
                .build()
                .unwrap();
            assert!(MetricsTracerImp::is_queue(&element));
        }
        let element = gst::ElementFactory::make("identity")
            .name("queue")
            .build()
            .unwrap();
        assert!(!MetricsTracerImp::is_queue(&element));
    }

    #[test]
    fn given_inflight_span_when_cache_cleared_then_snapshot_survives_until_post() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("identity")
            .name("cache_source")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("identity")
            .name("cache_sink")
            .build()
            .unwrap();
        let src_pad = source.static_pad("src").unwrap();
        let sink_pad = sink.static_pad("sink").unwrap();
        let source_refs = source.ref_count();
        let sink_refs = sink.ref_count();
        unsafe {
            let cache = MetricsTracerImp::do_create_latency_cache_for_pad_pair(
                src_pad.to_glib_none().0,
                sink_pad.to_glib_none().0,
                1,
            )
            .unwrap();
            assert_eq!(source.ref_count(), source_refs);
            assert_eq!(sink.ref_count(), sink_refs);
            assert!(MetricsTracerImp::replace_pad_cache(
                src_pad.as_ptr(),
                None,
                Some(&cache),
            ));
            drop(cache);
            let snapshot = std::rc::Rc::new(
                MetricsTracerImp::pad_cache(src_pad.to_glib_none().0)
                    .unwrap()
                    .as_ref()
                    .clone(),
            );
            let weak = std::rc::Rc::downgrade(&snapshot);
            // Parent forces accounting even when a test environment filters this element.
            super::THREAD_STATE.with(|state| {
                state.spans.borrow_mut().push(SpanFrame {
                    cache: Some(snapshot),
                    pad: src_pad.to_glib_none().0,
                    start_wall_ns: gst::get_timestamp().nseconds(),
                    start_cpu_ns: None,
                    child_wall_ns: 0,
                    child_cpu_ns: 0,
                    child_cpu_valid: true,
                    sampled: false,
                })
            });
            glib::gobject_ffi::g_object_set_qdata_full(
                src_pad.as_ptr().cast(),
                *super::PAD_CACHE_QUARK,
                std::ptr::null_mut(),
                None,
            );
            assert!(MetricsTracerImp::pad_cache(src_pad.to_glib_none().0).is_none());
            assert!(weak.upgrade().is_some());
            MetricsTracerImp::do_receive_and_record_latency_ts::<false, false>(
                0,
                src_pad.to_glib_none().0,
                false,
            );
            assert!(super::THREAD_STATE.with(|state| state.spans.borrow().is_empty()));
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn given_failed_cpu_clock_when_finishing_then_cpu_measurement_is_absent() {
        let pad = std::ptr::null_mut();
        let mut stack = vec![SpanFrame {
            cache: None,
            pad,
            start_wall_ns: 0,
            start_cpu_ns: Some(0),
            child_wall_ns: 0,
            child_cpu_ns: 0,
            child_cpu_valid: true,
            sampled: true,
        }];
        let (span, _) = MetricsTracerImp::complete_span(&mut stack, pad, 10, None).unwrap();
        assert_eq!(span.exclusive_wall_ns, 10);
        assert_eq!(span.exclusive_cpu_ns, None);
    }

    fn timing_frame(start_wall_ns: u64, start_cpu_ns: Option<u64>, sampled: bool) -> SpanFrame {
        SpanFrame {
            cache: None,
            pad: std::ptr::null_mut(),
            start_wall_ns,
            start_cpu_ns,
            child_wall_ns: 0,
            child_cpu_ns: 0,
            child_cpu_valid: true,
            sampled,
        }
    }

    #[test]
    fn given_unsampled_descendants_when_finishing_then_parent_cpu_stays_exclusive() {
        let pad = std::ptr::null_mut();
        let mut stack = vec![
            timing_frame(0, Some(0), true),
            // Only this direct child's inclusive CPU is needed by the parent.
            timing_frame(10, Some(5), false),
            timing_frame(20, None, false),
            // A deeper span can still be sampled independently.
            timing_frame(30, Some(15), true),
        ];
        let (leaf, _) = MetricsTracerImp::complete_span(&mut stack, pad, 40, Some(20)).unwrap();
        assert_eq!(leaf.exclusive_wall_ns, 10);
        assert_eq!(leaf.exclusive_cpu_ns, Some(5));
        let (grandchild, _) = MetricsTracerImp::complete_span(&mut stack, pad, 50, None).unwrap();
        assert_eq!(grandchild.exclusive_wall_ns, 20);
        assert_eq!(grandchild.exclusive_cpu_ns, None);
        let (child, _) = MetricsTracerImp::complete_span(&mut stack, pad, 80, Some(35)).unwrap();
        assert_eq!(child.exclusive_wall_ns, 40);
        assert_eq!(child.exclusive_cpu_ns, None);
        let (parent, _) = MetricsTracerImp::complete_span(&mut stack, pad, 100, Some(50)).unwrap();
        assert_eq!(parent.exclusive_wall_ns, 30);
        assert_eq!(parent.exclusive_cpu_ns, Some(20));
        assert!(stack.is_empty());
    }

    #[test]
    fn given_failed_grandchild_clock_when_finishing_then_only_direct_parent_loses_cpu_sample() {
        let pad = std::ptr::null_mut();
        for (start, end) in [(None, Some(20)), (Some(10), None), (None, None)] {
            let mut stack = vec![
                timing_frame(0, Some(0), true),
                timing_frame(10, Some(5), true),
                timing_frame(20, start, true),
            ];
            let (grandchild, _) =
                MetricsTracerImp::complete_span(&mut stack, pad, 40, end).unwrap();
            assert_eq!(grandchild.exclusive_cpu_ns, None);
            // A later successful sibling must not hide the missing child time.
            stack.push(timing_frame(50, Some(25), true));
            MetricsTracerImp::complete_span(&mut stack, pad, 70, Some(30));
            let (child, _) =
                MetricsTracerImp::complete_span(&mut stack, pad, 80, Some(35)).unwrap();
            assert_eq!(child.exclusive_wall_ns, 30);
            assert_eq!(child.exclusive_cpu_ns, None);
            let (parent, _) =
                MetricsTracerImp::complete_span(&mut stack, pad, 100, Some(50)).unwrap();
            assert_eq!(parent.exclusive_wall_ns, 30);
            assert_eq!(parent.exclusive_cpu_ns, Some(20));
            assert!(stack.is_empty());
        }
    }

    #[test]
    fn given_child_time_when_computing_exclusive_latency_then_it_subtracts_and_saturates() {
        assert_eq!(MetricsTracerImp::compute_element_latency(100, 30), 70);
        assert_eq!(MetricsTracerImp::compute_element_latency(30, 50), 0);
    }

    #[test]
    fn given_latency_buckets_when_inspecting_short_stages_then_microseconds_are_represented() {
        let buckets = latency_buckets();
        assert!(buckets.contains(&0.000_001));
        assert!(buckets.contains(&0.000_1));
        assert!(buckets.contains(&0.001));
        assert!(buckets.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn given_sampling_when_hashing_sequences_then_samples_are_spread() {
        let samples = (0..1024)
            .filter(|sequence| MetricsTracerImp::sample_sequence(*sequence, 0x1234, 16))
            .count();
        assert!((40..90).contains(&samples), "sample count was {samples}");
        let samples = (0..16384)
            .step_by(96)
            .filter(|sequence| MetricsTracerImp::sample_sequence(*sequence, 0x1234, 17))
            .count();
        assert!((3..20).contains(&samples), "sample count was {samples}");
    }

    #[test]
    fn given_element_patterns_when_matching_then_exact_prefix_suffix_and_contains_work() {
        assert!(MetricsTracerImp::glob_matches("rdinfer*", "rdinfer-async"));
        assert!(MetricsTracerImp::glob_matches("*queue", "infer-queue"));
        assert!(MetricsTracerImp::glob_matches("*infer*", "bench-rdinfer"));
        assert!(MetricsTracerImp::glob_matches("tracker", "tracker"));
        assert!(!MetricsTracerImp::glob_matches("tracker", "tracker-copy"));
    }

    #[test]
    fn given_two_sequential_children_when_parent_finishes_then_both_are_subtracted() {
        let mut parent_storage = 0u8;
        let mut first_child_storage = 0u8;
        let mut second_child_storage = 0u8;
        let parent_pad = (&mut parent_storage as *mut u8).cast::<gst::ffi::GstPad>();
        let first_child_pad = (&mut first_child_storage as *mut u8).cast::<gst::ffi::GstPad>();
        let second_child_pad = (&mut second_child_storage as *mut u8).cast::<gst::ffi::GstPad>();
        let mut stack = vec![SpanFrame {
            cache: None,
            pad: parent_pad,
            start_wall_ns: 0,
            start_cpu_ns: Some(0),
            child_wall_ns: 0,
            child_cpu_ns: 0,
            child_cpu_valid: true,
            sampled: true,
        }];

        stack.push(SpanFrame {
            cache: None,
            pad: first_child_pad,
            start_wall_ns: 10,
            start_cpu_ns: Some(5),
            child_wall_ns: 0,
            child_cpu_ns: 0,
            child_cpu_valid: true,
            sampled: true,
        });
        MetricsTracerImp::complete_span(&mut stack, first_child_pad, 30, Some(15));
        stack.push(SpanFrame {
            cache: None,
            pad: second_child_pad,
            start_wall_ns: 35,
            start_cpu_ns: Some(20),
            child_wall_ns: 0,
            child_cpu_ns: 0,
            child_cpu_valid: true,
            sampled: true,
        });
        MetricsTracerImp::complete_span(&mut stack, second_child_pad, 65, Some(35));

        assert_eq!(
            MetricsTracerImp::complete_span(&mut stack, parent_pad, 100, Some(50))
                .map(|(span, _)| span),
            Some(CompletedSpan {
                exclusive_wall_ns: 50,
                exclusive_cpu_ns: Some(25),
                sampled: true,
            })
        );
    }

    #[test]
    fn given_mismatched_post_hook_when_finishing_then_stack_is_cleared() {
        let mut expected_storage = 0u8;
        let mut received_storage = 0u8;
        let expected_pad = (&mut expected_storage as *mut u8).cast::<gst::ffi::GstPad>();
        let received_pad = (&mut received_storage as *mut u8).cast::<gst::ffi::GstPad>();
        let mut stack = vec![SpanFrame {
            cache: None,
            pad: expected_pad,
            start_wall_ns: 0,
            start_cpu_ns: None,
            child_wall_ns: 0,
            child_cpu_ns: 0,
            child_cpu_valid: true,
            sampled: false,
        }];

        assert_eq!(
            MetricsTracerImp::complete_span(&mut stack, received_pad, 10, None)
                .map(|(span, _)| span),
            None
        );
        assert!(stack.is_empty());
    }
}
