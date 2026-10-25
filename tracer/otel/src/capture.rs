//! Streaming-thread capture: cached descriptors, owned tokens, and a reusable stack.
use std::cell::RefCell;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, LazyLock};

use gstreamer::{self as gst, prelude::*};
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};

use crate::export::{text_value, Runtime};

const MAX_DEPTH: usize = 256;
static IDS: LazyLock<RandomIdGenerator> = LazyLock::new(RandomIdGenerator::default);

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub flags: TraceFlags,
    pub is_remote: bool,
    pub trace_state: Option<Arc<TraceState>>,
}
impl Token {
    pub fn rejected() -> Self {
        Self {
            trace_id: TraceId::INVALID,
            span_id: SpanId::INVALID,
            flags: TraceFlags::default(),
            is_remote: false,
            trace_state: None,
        }
    }
    pub fn from_context(context: &SpanContext) -> Self {
        Self {
            trace_id: context.trace_id(),
            span_id: context.span_id(),
            flags: context.trace_flags(),
            is_remote: context.is_remote(),
            trace_state: (context.trace_state() != &TraceState::default())
                .then(|| Arc::new(context.trace_state().clone())),
        }
    }
    pub fn sampled(&self) -> bool {
        self.flags.is_sampled()
    }
    pub fn context(&self) -> SpanContext {
        SpanContext::new(
            self.trace_id,
            self.span_id,
            self.flags,
            self.is_remote,
            self.trace_state.as_deref().cloned().unwrap_or_default(),
        )
    }
}

pub(crate) use crate::metadata::Site;

pub(crate) struct ThreadInfo {
    pub encoded: std::sync::OnceLock<crate::wire::EncodedAttributes>,
    pub id: Arc<str>,
    pub name: Arc<str>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Push,
    Pull,
    PushList,
}
impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Push => "gst.pad.push",
            Self::Pull => "gst.pad.pull_range",
            Self::PushList => "gst.pad.push_list",
        }
    }
    pub fn error(self, flow_return: i32) -> Option<&'static str> {
        // A pull at the end of a stream normally returns EOS without a buffer.
        (flow_return < 0 && !(self == Self::Pull && flow_return == gst::ffi::GST_FLOW_EOS))
            .then_some(match self {
                Self::Push => "GStreamer push failed",
                Self::Pull => "GStreamer pull failed",
                Self::PushList => "GStreamer buffer-list push failed",
            })
    }
}
#[cfg_attr(test, derive(Clone))]
pub(crate) struct Record {
    pub operation: Operation,
    pub site: Arc<Site>,
    pub token: Token,
    pub parent: SpanId,
    pub parent_is_remote: bool,
    pub start: u64,
    pub end: u64,
    pub buffer_id: usize,
    pub buffer_size: usize,
    pub list_len: Option<usize>,
    pub links: Option<Arc<crate::propagation::Links>>,
    pub thread: Arc<ThreadInfo>,
    pub flow_return: i32,
    pub profiled: bool,
}
struct Frame {
    pad: usize,
    operation: Operation,
    token: Token,
    recording: bool,
    timing: Option<crate::metrics::Timing>,
}
#[derive(Default)]
struct ThreadState {
    frames: Vec<Frame>,
    sampled: Vec<Record>,
    overflow: usize,
    info: Option<Arc<ThreadInfo>>,
    profile_invalid: bool,
    sequence: u64,
}
thread_local! {
    static LOCAL: RefCell<ThreadState> = const { RefCell::new(ThreadState {
        frames: Vec::new(),
        sampled: Vec::new(),
        overflow: 0,
        info: None,
        profile_invalid: false,
        sequence: 0,
    }) };
}

fn root(ratio: f64) -> Token {
    // External unsampled contexts must remain unsampled, including at sources.
    if let Some(parent) = opentelemetry::Context::map_current(|cx| {
        let span = cx.span();
        span.span_context()
            .is_valid()
            .then(|| Token::from_context(span.span_context()))
    }) {
        return parent;
    }
    if ratio == 0.0 {
        return Token::rejected();
    }
    let trace_id = IDS.new_trace_id();
    let bytes = trace_id.to_bytes();
    let random = u64::from_be_bytes(bytes[8..].try_into().unwrap()) >> 1;
    let sampled = ratio >= 1.0 || random < (ratio * (1u64 << 63) as f64) as u64;
    Token {
        trace_id,
        span_id: SpanId::INVALID,
        flags: TraceFlags::default().with_sampled(sampled),
        is_remote: false,
        trace_state: None,
    }
}

pub(crate) fn current_context() -> Option<SpanContext> {
    LOCAL.with(|local| {
        local
            .try_borrow()
            .ok()
            .and_then(|s| s.frames.last().map(|f| f.token.context()))
    })
}

pub(crate) fn begin(runtime: &Runtime, ts: u64, pad: &gst::Pad, buffer: &gst::Buffer) {
    LOCAL.with(|local| begin_local(local, runtime, ts, pad, Some(buffer), Operation::Push, None));
}

pub(crate) fn begin_pull(runtime: &Runtime, ts: u64, pad: &gst::Pad) {
    LOCAL.with(|local| begin_local(local, runtime, ts, pad, None, Operation::Pull, None));
}

pub(crate) fn begin_list(runtime: &Runtime, ts: u64, pad: &gst::Pad, list: &gst::BufferList) {
    use glib::translate::from_glib_borrow;
    // The list owns its buffers throughout the hook. Borrow wrappers do not
    // increment refcounts or change the normal metadata writability decision.
    let first: Option<glib::translate::Borrowed<gst::Buffer>> = list
        .get(0)
        .map(|buffer| unsafe { from_glib_borrow(buffer.as_ptr() as *mut gst::ffi::GstBuffer) });
    LOCAL.with(|local| {
        begin_local(
            local,
            runtime,
            ts,
            pad,
            first.as_deref(),
            Operation::PushList,
            Some(list),
        )
    });
}

fn begin_local(
    local: &RefCell<ThreadState>,
    runtime: &Runtime,
    ts: u64,
    pad: &gst::Pad,
    buffer: Option<&gst::Buffer>,
    operation: Operation,
    list: Option<&gst::BufferList>,
) {
    crate::profiles::clear();
    // Resolve TLS once. Individual borrows still end before GStreamer calls,
    // allowing callbacks and the optional log bridge to inspect the context.
    let (overflow, inherited) = {
        let mut state = local.borrow_mut();
        if state.overflow > 0 || state.frames.len() == MAX_DEPTH {
            state.overflow += 1;
            if let Some(timing) = state.frames.last_mut().and_then(|f| f.timing.as_mut()) {
                timing.valid = false;
            }
            (true, None)
        } else {
            (false, state.frames.last().map(|frame| frame.token.clone()))
        }
    };
    if overflow {
        runtime.stats.local().dropped_depth.fetch_add(1, Relaxed);
        return;
    }
    // A real source starts a new buffer journey, even when a buffer was recycled.
    // A top-level pull has no input buffer: the caller is asking upstream to
    // produce one. Nested pulls share the active decision, including rejection.
    let is_root = runtime.config.traces
        && inherited.is_none()
        && (operation == Operation::Pull || crate::metadata::is_source(pad));
    let parent_token = if !runtime.config.traces {
        Token::rejected()
    } else {
        inherited.unwrap_or_else(|| {
            if is_root {
                root(runtime.config.ratio)
            } else {
                buffer
                    .and_then(crate::propagation::read)
                    .unwrap_or_else(|| {
                        runtime
                            .stats
                            .local()
                            .propagation_misses
                            .fetch_add(1, Relaxed);
                        Token::rejected()
                    })
            }
        })
    };
    // Read fan-in evidence before replacing the input metadata with this span.
    // Sources can recycle buffers; stale metadata must not become input links.
    let recording = parent_token.sampled() && runtime.accepting();
    let links = if recording && !is_root {
        let mut links = crate::propagation::Links::default();
        if let Some(list) = list {
            for input in list.iter().filter_map(crate::propagation::read_input) {
                links.extend(&input, &parent_token);
            }
        } else if let Some(input) = buffer.and_then(|b| crate::propagation::read_fan_in(b)) {
            // Single-input shared branches retain a common ancestor; do not
            // invent a link to it on every synchronous downstream call.
            links.extend(&input, &parent_token);
        }
        links.finish()
    } else {
        None
    };
    let parent = parent_token.span_id;
    let parent_is_remote = parent_token.is_remote;
    let mut token = parent_token;
    let site = (runtime.metrics.is_some() || recording)
        .then(|| crate::metadata::site(pad, runtime.config.value_length_limit));
    let series = runtime
        .metrics
        .as_ref()
        .filter(|_| operation != Operation::Pull)
        .and_then(|m| m.series(site.as_ref().unwrap()));
    let record = if recording {
        let site = site.unwrap();
        token.span_id = IDS.new_span_id();
        token.is_remote = false;
        runtime.anchor(ts);
        runtime.stats.local().started_spans.fetch_add(1, Relaxed);
        let thread = {
            let mut state = local.borrow_mut();
            state
                .info
                .get_or_insert_with(|| {
                    let thread = std::thread::current();
                    Arc::new(ThreadInfo {
                        encoded: Default::default(),
                        id: text_value(
                            &format!("{:?}", thread.id()),
                            runtime.config.value_length_limit,
                        ),
                        name: text_value(
                            thread.name().unwrap_or("unnamed"),
                            runtime.config.value_length_limit,
                        ),
                    })
                })
                .clone()
        };
        Some(Record {
            operation,
            site,
            token: token.clone(),
            parent,
            parent_is_remote,
            start: ts,
            end: ts,
            buffer_id: list.map_or_else(
                || buffer.map_or(0, |b| b.as_ptr() as usize),
                |l| l.as_ptr() as usize,
            ),
            buffer_size: list
                .map_or_else(|| buffer.map_or(0, |b| b.size()), |l| l.calculate_size()),
            list_len: list.map(|l| l.len()),
            links,
            thread,
            flow_return: 0,
            profiled: false,
        })
    } else {
        if token.sampled() {
            runtime.stats.local().skipped_closed.fetch_add(1, Relaxed);
        } else if runtime.config.traces {
            let stats = runtime.stats.local();
            match operation {
                Operation::Push => &stats.unsampled_pushes,
                Operation::Pull => &stats.unsampled_pulls,
                Operation::PushList => &stats.unsampled_lists,
            }
            .fetch_add(1, Relaxed);
        }
        None
    };
    if runtime.config.traces {
        if let Some(list) = list {
            for buffer in list.iter() {
                // SAFETY: each borrowed buffer remains owned by the input list.
                let buffer: glib::translate::Borrowed<gst::Buffer> = unsafe {
                    glib::translate::from_glib_borrow(buffer.as_ptr() as *mut gst::ffi::GstBuffer)
                };
                crate::propagation::write(&buffer, &token, is_root, &runtime.stats);
            }
        } else if let Some(buffer) = buffer {
            crate::propagation::write(buffer, &token, is_root, &runtime.stats);
        }
    }
    {
        let mut state = local.borrow_mut();
        let recording = record.is_some();
        if let Some(record) = record {
            state.sampled.push(record);
        }
        state.frames.push(Frame {
            pad: pad.as_ptr() as usize,
            operation,
            token,
            recording,
            timing: None,
        });
        if publish_profile(&state) {
            if let Some(record) = state.sampled.last_mut() {
                record.profiled = true;
            }
        }
        if let Some(metrics) = &runtime.metrics {
            let sampled =
                series.is_some() && metrics.cpu_sampled(state.sequence, pad.as_ptr() as usize);
            state.sequence = state.sequence.wrapping_add(1);
            let parent_sampled = state
                .frames
                .iter()
                .rev()
                .nth(1)
                .and_then(|f| f.timing.as_ref())
                .is_some_and(|t| t.sample_cpu);
            let cpu = (sampled || parent_sampled)
                .then(crate::metrics::cpu_time)
                .flatten();
            state.frames.last_mut().unwrap().timing = Some(crate::metrics::Timing::new(
                series,
                gst::get_timestamp().nseconds(),
                cpu,
                sampled,
            ));
        }
    }
}

fn publish_profile(state: &ThreadState) -> bool {
    let token = state
        .frames
        .last()
        .filter(|frame| frame.recording && state.overflow == 0 && !state.profile_invalid)
        .map(|frame| &frame.token);
    crate::profiles::publish(token)
}

pub(crate) fn end(runtime: &Runtime, ts: u64, pad: &gst::Pad, flow_return: i32) -> Option<Token> {
    finish(runtime, ts, pad, flow_return, Operation::Push, None)
}

pub(crate) fn end_list(
    runtime: &Runtime,
    ts: u64,
    pad: &gst::Pad,
    flow_return: i32,
) -> Option<Token> {
    finish(runtime, ts, pad, flow_return, Operation::PushList, None)
}

pub(crate) fn end_pull(
    runtime: &Runtime,
    ts: u64,
    pad: &gst::Pad,
    flow_return: i32,
    buffer: Option<&gst::Buffer>,
) -> Option<Token> {
    finish(runtime, ts, pad, flow_return, Operation::Pull, buffer)
}

fn finish(
    runtime: &Runtime,
    ts: u64,
    pad: &gst::Pad,
    flow_return: i32,
    operation: Operation,
    buffer: Option<&gst::Buffer>,
) -> Option<Token> {
    let end = runtime
        .metrics
        .as_ref()
        .map(|_| gst::get_timestamp().nseconds());
    crate::profiles::clear();
    let completed = LOCAL.with(|local| {
        let mut state = local.borrow_mut();
        if state.overflow > 0 {
            state.overflow -= 1;
            publish_profile(&state);
            return None;
        }
        if state
            .frames
            .last()
            .is_none_or(|f| f.pad != pad.as_ptr() as usize || f.operation != operation)
        {
            runtime.stats.local().mismatched_hooks.fetch_add(1, Relaxed);
            state.profile_invalid = !state.frames.is_empty();
            for frame in &mut state.frames {
                if let Some(timing) = &mut frame.timing {
                    timing.valid = false;
                }
            }
            return None;
        }
        let frame = state.frames.pop().unwrap();
        let cpu = frame
            .timing
            .as_ref()
            .filter(|t| t.start_cpu.is_some())
            .and_then(|_| crate::metrics::cpu_time());
        if let (Some(parent), Some(child)) = (
            state.frames.last_mut().and_then(|f| f.timing.as_mut()),
            frame.timing.as_ref(),
        ) {
            parent.finish_child(child, end.unwrap(), cpu);
        }
        let record = if frame.recording {
            state.sampled.pop()
        } else {
            None
        };
        if state.frames.is_empty() {
            state.profile_invalid = false;
        }
        publish_profile(&state);
        Some((record, frame, cpu))
    });
    if let Some((record, frame, cpu)) = completed {
        if operation == Operation::Pull && flow_return >= gst::ffi::GST_FLOW_OK {
            if let Some(buffer) = buffer {
                // Replace recycled/nested-read context with the completed read's
                // decision. No thread-local continuation survives this call:
                // subsequent pushes must receive this buffer's owned metadata.
                crate::propagation::write(buffer, &frame.token, true, &runtime.stats);
            }
        }
        let accepted = if let Some(mut record) = record {
            record.end = ts;
            record.flow_return = flow_return;
            if let Some(buffer) = buffer {
                record.buffer_id = buffer.as_ptr() as usize;
                record.buffer_size = buffer.size();
            }
            runtime.submit(record)
        } else {
            false
        };
        if let (Some(metrics), Some(timing)) = (&runtime.metrics, frame.timing) {
            metrics.observe(timing, end.unwrap(), cpu, accepted.then_some(&frame.token));
        }
        return accepted.then_some(frame.token);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{
        tests::{runtime, Collector},
        Config,
    };
    use opentelemetry::KeyValue;
    use std::time::Duration;

    #[test]
    fn mixed_input_lists_and_aggregated_metadata_export_links_without_resampling() {
        gst::init().unwrap();
        let identity = gst::ElementFactory::make("identity").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        identity.link(&sink).unwrap();
        let pad = identity.static_pad("src").unwrap();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        let buffers: Vec<_> = (1..=12)
            .map(|id| {
                let buffer = gst::Buffer::new();
                let token = Token {
                    trace_id: TraceId::from(id as u128),
                    span_id: SpanId::from(id),
                    flags: TraceFlags::SAMPLED,
                    is_remote: false,
                    trace_state: None,
                };
                crate::propagation::write(&buffer, &token, true, &runtime.stats);
                buffer
            })
            .collect();
        let mut merged = gst::Buffer::new();
        let refs: Vec<_> = buffers.iter().map(|b| b.as_ref()).collect();
        crate::propagation::propagate_input_contexts(merged.get_mut().unwrap(), &refs);
        begin(&runtime, 100, &pad, &merged);
        end(&runtime, 120, &pad, 0);
        // Metadata was consumed, so a later hop does not repeat the input links.
        begin(&runtime, 125, &pad, &merged);
        end(&runtime, 130, &pad, 0);
        let mut list = gst::BufferList::new();
        for buffer in buffers {
            list.get_mut().unwrap().add(buffer);
        }
        begin_list(&runtime, 140, &pad, &list);
        end_list(&runtime, 160, &pad, 0);
        // A missing or rejected first member cannot be resampled from a later one.
        let rejected = gst::Buffer::new();
        crate::propagation::write(&rejected, &Token::rejected(), true, &runtime.stats);
        list.get_mut().unwrap().insert(0, rejected);
        begin_list(&runtime, 170, &pad, &list);
        end_list(&runtime, 180, &pad, 0);
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.exported_spans, 3);
        assert_eq!(stats.unsampled_lists, 1);
        let spans = collector.0.lock().unwrap();
        for index in [0, 2] {
            assert_eq!(spans[index].parent_span_id, SpanId::from(1));
            assert_eq!(spans[index].links.len(), crate::propagation::MAX_LINKS);
            assert_eq!(spans[index].links.dropped_count, 3);
            assert_eq!(
                spans[index].links[0].span_context.trace_id(),
                TraceId::from(2)
            );
        }
        assert!(spans[1].links.is_empty());
        assert_eq!(spans[1].parent_span_id, spans[0].span_context.span_id());
    }

    #[test]
    fn list_roots_replace_shared_context_and_empty_or_failed_lists_remain_balanced() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        source.link(&sink).unwrap();
        let pad = source.static_pad("src").unwrap();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        let buffer = gst::Buffer::with_size(4).unwrap();
        let mut list = gst::BufferList::new();
        list.get_mut().unwrap().add(buffer.clone());
        let shared = list.clone();
        begin_list(&runtime, 100, &pad, &list);
        let first = current_context().unwrap();
        assert_eq!(crate::propagation::read(&buffer).unwrap().context(), first);
        end_list(&runtime, 120, &pad, 0);
        begin_list(&runtime, 130, &pad, &shared);
        let next = current_context().unwrap();
        assert_ne!(first.trace_id(), next.trace_id());
        assert_eq!(crate::propagation::read(&buffer).unwrap().context(), next);
        end_list(&runtime, 150, &pad, gst::ffi::GST_FLOW_ERROR);
        begin_list(&runtime, 160, &pad, &gst::BufferList::new());
        end_list(&runtime, 180, &pad, 0);
        assert!(current_context().is_none());
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.exported_spans, 3);
        assert_eq!(stats.mismatched_hooks + stats.propagation_misses, 0);
        let spans = collector.0.lock().unwrap();
        assert!(spans.iter().all(|s| s.name == "gst.pad.push_list fakesink"));
        assert!(matches!(
            spans[1].status,
            opentelemetry::trace::Status::Error { .. }
        ));
        assert!(spans[2]
            .attributes
            .contains(&KeyValue::new("buffer.list.length", 0_i64)));
    }

    #[test]
    fn nested_pulls_seed_owned_context_for_downstream_pushes() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc")
            .name("source")
            .build()
            .unwrap();
        let middle = gst::ElementFactory::make("identity")
            .name("middle")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .name("sink")
            .build()
            .unwrap();
        gst::Element::link_many([&source, &middle, &sink]).unwrap();
        let outer = sink.static_pad("sink").unwrap();
        let inner = middle.static_pad("sink").unwrap();
        let downstream = middle.static_pad("src").unwrap();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        let buffer = gst::Buffer::with_size(64).unwrap();
        begin_pull(&runtime, 100, &outer);
        let root = current_context().unwrap();
        begin_pull(&runtime, 110, &inner);
        let child = current_context().unwrap();
        end_pull(&runtime, 120, &inner, 0, Some(&buffer));
        assert_eq!(
            crate::propagation::read(&buffer).unwrap().span_id,
            child.span_id()
        );
        // The outer read must replace nested-read metadata even after sharing.
        let shared = buffer.clone();
        end_pull(&runtime, 130, &outer, 0, Some(&buffer));
        assert!(current_context().is_none());
        assert_eq!(
            crate::propagation::read(&shared).unwrap().span_id,
            root.span_id()
        );
        let copied = buffer.copy();
        begin(&runtime, 140, &downstream, &copied);
        end(&runtime, 150, &downstream, 0);
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.propagation_misses, 0);
        assert_eq!(stats.mismatched_hooks, 0);
        let spans = collector.0.lock().unwrap();
        assert_eq!(spans.len(), 3);
        for span in spans.iter() {
            assert_eq!(span.span_context.trace_id(), root.trace_id());
            assert_eq!(
                span.parent_span_id,
                if span.span_context.span_id() == root.span_id() {
                    SpanId::INVALID
                } else {
                    root.span_id()
                }
            );
            if span.span_context.span_id() == child.span_id() {
                assert!(
                    span.name.starts_with("gst.pad.pull_range "),
                    "{}",
                    span.name
                );
                assert!(span
                    .attributes
                    .contains(&opentelemetry::KeyValue::new("src_pad.element", "source")));
                assert!(span
                    .attributes
                    .contains(&opentelemetry::KeyValue::new("sink_pad.element", "middle")));
                assert!(span
                    .attributes
                    .contains(&opentelemetry::KeyValue::new("buffer.size", 64_i64)));
            }
        }
    }

    #[test]
    fn rejected_pulls_replace_recycled_context_without_resampling() {
        gst::init().unwrap();
        let middle = gst::ElementFactory::make("identity").build().unwrap();
        let input = middle.static_pad("sink").unwrap();
        let output = middle.static_pad("src").unwrap();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        let buffer = gst::Buffer::new();
        begin_pull(&runtime, 1, &input);
        end_pull(&runtime, 2, &input, 0, Some(&buffer));
        assert!(crate::propagation::read(&buffer).unwrap().sampled());
        let parent = SpanContext::new(
            TraceId::from(42),
            SpanId::from(9),
            TraceFlags::default(),
            true,
            TraceState::default(),
        );
        {
            let _guard = opentelemetry::Context::new()
                .with_remote_span_context(parent.clone())
                .attach();
            begin_pull(&runtime, 3, &input);
            begin_pull(&runtime, 4, &input);
            end_pull(&runtime, 5, &input, 0, Some(&buffer));
            end_pull(&runtime, 6, &input, 0, Some(&buffer));
        }
        assert!(current_context().is_none());
        let token = crate::propagation::read(&buffer).unwrap();
        assert!(!token.sampled());
        assert_eq!(token.trace_id, parent.trace_id());
        begin(&runtime, 7, &output, &buffer.copy());
        end(&runtime, 8, &output, 0);
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.unsampled_pulls, 2);
        assert_eq!(stats.unsampled_pushes, 1);
        assert_eq!(stats.propagation_misses, 0);
        assert_eq!(collector.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn pull_eos_is_normal_but_read_errors_are_reported() {
        gst::init().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        let pad = sink.static_pad("sink").unwrap();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        for flow in [gst::ffi::GST_FLOW_EOS, gst::ffi::GST_FLOW_ERROR] {
            begin_pull(&runtime, 1, &pad);
            end_pull(&runtime, 2, &pad, flow, None);
            assert!(current_context().is_none());
        }
        runtime.shutdown(Duration::from_secs(2)).unwrap();
        let spans = collector.0.lock().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].status, opentelemetry::trace::Status::Unset);
        assert!(matches!(
            spans[1].status,
            opentelemetry::trace::Status::Error { .. }
        ));
        assert_ne!(
            spans[0].span_context.trace_id(),
            spans[1].span_context.trace_id()
        );
    }

    #[test]
    fn external_parent_decision_and_trace_state_are_preserved() {
        let trace_state = TraceState::from_key_value([("vendor", "entry")]).unwrap();
        for sampled in [false, true] {
            let context = SpanContext::new(
                TraceId::from(42),
                SpanId::from(9),
                TraceFlags::default().with_sampled(sampled),
                true,
                trace_state.clone(),
            );
            let _guard = opentelemetry::Context::new()
                .with_remote_span_context(context.clone())
                .attach();
            for ratio in [0.0, 0.5, 1.0] {
                let token = root(ratio);
                assert_eq!(token.sampled(), sampled);
                assert_eq!(token.trace_id, context.trace_id());
                assert_eq!(token.span_id, context.span_id());
                assert!(token.context().is_remote());
                assert_eq!(token.context().trace_state(), context.trace_state());
            }
        }
        assert!(!root(0.0).sampled());
        assert!(root(1.0).sampled());
    }

    #[test]
    fn remote_parent_applies_only_to_the_first_local_span() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let downstream = gst::ElementFactory::make("identity").build().unwrap();
        let source_pad = source.static_pad("src").unwrap();
        let downstream_pad = downstream.static_pad("src").unwrap();
        let parent = SpanContext::new(
            TraceId::from(42),
            SpanId::from(9),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let _guard = opentelemetry::Context::new()
            .with_remote_span_context(parent.clone())
            .attach();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        let buffer = gst::Buffer::new();
        begin(&runtime, 100, &source_pad, &buffer);
        let local = current_context().unwrap();
        assert!(!local.is_remote());
        begin(&runtime, 110, &downstream_pad, &buffer);
        end(&runtime, 120, &downstream_pad, 0);
        end(&runtime, 130, &source_pad, 0);
        runtime.shutdown(Duration::from_secs(2)).unwrap();
        let spans = collector.0.lock().unwrap();
        assert_eq!(spans.len(), 2);
        for span in spans.iter() {
            let first_local = span.span_context.span_id() == local.span_id();
            assert_eq!(span.span_context.trace_id(), parent.trace_id());
            assert_eq!(span.parent_span_is_remote, first_local);
            assert_eq!(
                span.parent_span_id,
                if first_local {
                    parent.span_id()
                } else {
                    local.span_id()
                }
            );
            let proto: opentelemetry_proto::tonic::trace::v1::Span = span.clone().into();
            assert_eq!(proto.flags & 0x300, if first_local { 0x300 } else { 0x100 });
        }
    }

    #[test]
    fn missing_async_context_is_not_resampled_and_failed_push_is_recorded() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let downstream = gst::ElementFactory::make("identity").build().unwrap();
        let source_pad = source.static_pad("src").unwrap();
        let downstream_pad = downstream.static_pad("src").unwrap();
        let collector = Collector::default();
        let runtime = runtime(Config::default(), collector.clone());
        let buffer = gst::Buffer::new();
        begin(&runtime, 100, &downstream_pad, &buffer);
        assert!(!current_context().unwrap().is_sampled());
        end(&runtime, 101, &downstream_pad, 0);
        begin(&runtime, 110, &source_pad, &buffer);
        assert!(current_context().unwrap().is_sampled());
        end(&runtime, 120, &source_pad, -1);
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.propagation_misses, 1);
        assert_eq!(stats.unsampled_pushes, 1);
        assert_eq!(stats.exported_spans, 1);
        assert!(matches!(
            collector.0.lock().unwrap()[0].status,
            opentelemetry::trace::Status::Error { .. }
        ));
        assert!(current_context().is_none());
        begin(&runtime, 130, &source_pad, &buffer);
        end(&runtime, 140, &source_pad, 0);
        assert_eq!(runtime.stats.snapshot().skipped_closed, 1);
        assert_eq!(runtime.stats.snapshot().unsampled_pushes, 1);
    }

    #[test]
    fn nesting_is_bounded_and_hook_stack_recovers_after_overflow() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let pad = source.static_pad("src").unwrap();
        let buffer = gst::Buffer::new();
        let runtime = runtime(
            Config {
                ratio: 0.0,
                ..Config::default()
            },
            Collector::default(),
        );
        for _ in 0..MAX_DEPTH + 10 {
            begin(&runtime, 100, &pad, &buffer);
        }
        for _ in 0..MAX_DEPTH + 10 {
            end(&runtime, 200, &pad, 0);
        }
        assert!(current_context().is_none());
        assert!(!crate::metadata::has_description(&pad));
        assert_eq!(runtime.stats.snapshot().dropped_depth, 10);
        assert_eq!(runtime.stats.snapshot().mismatched_hooks, 0);
        runtime.shutdown(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn sampled_stack_recovers_from_overflow_and_closed_nested_pushes() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let pad = source.static_pad("src").unwrap();
        let buffer = gst::Buffer::new();
        let runtime = runtime(Config::default(), Collector::default());
        for ts in 0..MAX_DEPTH + 3 {
            begin(&runtime, ts as u64, &pad, &buffer);
        }
        for ts in MAX_DEPTH + 3..2 * (MAX_DEPTH + 3) {
            end(&runtime, ts as u64, &pad, 0);
        }
        assert!(current_context().is_none());
        assert_eq!(runtime.stats.snapshot().dropped_depth, 3);
        assert_eq!(
            runtime
                .flush(Duration::from_secs(2))
                .unwrap()
                .exported_spans,
            MAX_DEPTH as u64
        );
        begin(&runtime, 1000, &pad, &buffer);
        runtime.shutdown(Duration::from_secs(2)).unwrap();
        begin(&runtime, 1001, &pad, &buffer); // sampled token, no recording after close
        end(&runtime, 1002, &pad, 0);
        assert!(current_context().unwrap().is_sampled());
        end(&runtime, 1003, &pad, 0);
        assert!(current_context().is_none());
        let stats = runtime.stats.snapshot();
        assert_eq!(stats.started_spans, stats.completed_spans);
        assert_eq!(stats.dropped_closed, 1);
        assert_eq!(stats.skipped_closed, 1);
        assert_eq!(stats.mismatched_hooks, 0);
    }
}

#[cfg(test)]
mod sampling_metadata_tests {
    use super::*;
    use crate::export::{
        tests::{runtime, Collector},
        Config,
    };
    #[test]
    fn rejected_nested_pushes_never_materialize_descriptions() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let identity = gst::ElementFactory::make("identity").build().unwrap();
        let source_pad = source.static_pad("src").unwrap();
        let pad = identity.static_pad("src").unwrap();
        let buffer = gst::Buffer::new();
        let runtime = runtime(
            Config {
                ratio: 0.0,
                ..Config::default()
            },
            Collector::default(),
        );
        begin(&runtime, 1, &source_pad, &buffer);
        begin(&runtime, 2, &pad, &buffer);
        assert!(!crate::metadata::has_description(&source_pad));
        assert!(!crate::metadata::has_description(&pad));
        end(&runtime, 3, &pad, 0);
        end(&runtime, 4, &source_pad, 0);
        assert!(!crate::propagation::read(&buffer).unwrap().sampled());
        runtime.shutdown(std::time::Duration::from_secs(2)).unwrap();
    }
}

#[cfg(test)]
mod benchmarks {
    use super::*;
    use crate::export::{
        tests::{runtime, Collector},
        Config,
    };
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };

    #[test]
    #[ignore = "manual release benchmark; reports timings without timing assertions"]
    fn rejected_capture_benchmark() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let pad = source.static_pad("src").unwrap();
        let buffer = gst::Buffer::new();
        let runtime = runtime(
            Config {
                ratio: 0.0,
                ..Config::default()
            },
            Collector::default(),
        );
        let token = Token::rejected();
        begin(&runtime, 0, &pad, &buffer);
        end(&runtime, 1, &pad, 0);
        for round in 0..5 {
            let start = Instant::now();
            for _ in 0..500_000 {
                crate::propagation::write(
                    black_box(&buffer),
                    black_box(&token),
                    false,
                    &runtime.stats,
                );
            }
            println!(
                "CAPTURE round={round} stage=propagation operations=500000 elapsed_ns={}",
                start.elapsed().as_nanos()
            );
            let start = Instant::now();
            for _ in 0..100_000 {
                for ts in 0..5 {
                    begin(&runtime, ts, black_box(&pad), black_box(&buffer));
                }
                for ts in 5..10 {
                    end(&runtime, ts, black_box(&pad), 0);
                }
            }
            println!(
                "CAPTURE round={round} stage=nested_hooks operations=500000 elapsed_ns={}",
                start.elapsed().as_nanos()
            );
            assert!(current_context().is_none());
        }
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.unsampled_pushes, 2_500_001);
        assert_eq!(stats.propagation_misses, 0);
        assert_eq!(stats.mismatched_hooks, 0);
    }
}
