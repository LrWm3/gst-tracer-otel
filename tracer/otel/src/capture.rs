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
#[cfg_attr(test, derive(Clone))]
pub(crate) struct Record {
    pub site: Arc<Site>,
    pub token: Token,
    pub parent: SpanId,
    pub parent_is_remote: bool,
    pub start: u64,
    pub end: u64,
    pub buffer_id: usize,
    pub buffer_size: usize,
    pub thread: Arc<ThreadInfo>,
    pub flow_return: i32,
}
struct Frame {
    pad: usize,
    token: Token,
    recording: bool,
}
#[derive(Default)]
struct ThreadState {
    frames: Vec<Frame>,
    sampled: Vec<Record>,
    overflow: usize,
    info: Option<Arc<ThreadInfo>>,
}
thread_local! {
    static LOCAL: RefCell<ThreadState> = const { RefCell::new(ThreadState {
        frames: Vec::new(),
        sampled: Vec::new(),
        overflow: 0,
        info: None,
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
    LOCAL.with(|local| begin_local(local, runtime, ts, pad, buffer));
}

fn begin_local(
    local: &RefCell<ThreadState>,
    runtime: &Runtime,
    ts: u64,
    pad: &gst::Pad,
    buffer: &gst::Buffer,
) {
    // Resolve TLS once. Individual borrows still end before GStreamer calls,
    // allowing callbacks and the optional log bridge to inspect the context.
    let (overflow, inherited) = {
        let mut state = local.borrow_mut();
        if state.overflow > 0 || state.frames.len() == MAX_DEPTH {
            state.overflow += 1;
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
    let is_root = inherited.is_none() && crate::metadata::is_source(pad);
    let parent_token = inherited.unwrap_or_else(|| {
        if is_root {
            root(runtime.config.ratio)
        } else {
            crate::propagation::read(buffer).unwrap_or_else(|| {
                runtime
                    .stats
                    .local()
                    .propagation_misses
                    .fetch_add(1, Relaxed);
                Token::rejected()
            })
        }
    });
    let parent = parent_token.span_id;
    let parent_is_remote = parent_token.is_remote;
    let mut token = parent_token;
    let record = if token.sampled() && runtime.accepting() {
        let site = crate::metadata::site(pad, runtime.config.value_length_limit);
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
            site,
            token: token.clone(),
            parent,
            parent_is_remote,
            start: ts,
            end: ts,
            buffer_id: buffer.as_ptr() as usize,
            buffer_size: buffer.size(),
            thread,
            flow_return: 0,
        })
    } else {
        if token.sampled() {
            runtime.stats.local().skipped_closed.fetch_add(1, Relaxed);
        } else {
            runtime.stats.local().unsampled_pushes.fetch_add(1, Relaxed);
        }
        None
    };
    crate::propagation::write(buffer, &token, is_root, &runtime.stats);
    {
        let mut state = local.borrow_mut();
        let recording = record.is_some();
        if let Some(record) = record {
            state.sampled.push(record);
        }
        state.frames.push(Frame {
            pad: pad.as_ptr() as usize,
            token,
            recording,
        })
    }
}

pub(crate) fn end(runtime: &Runtime, ts: u64, pad: &gst::Pad, flow_return: i32) {
    let record = LOCAL.with(|local| {
        let mut state = local.borrow_mut();
        if state.overflow > 0 {
            state.overflow -= 1;
            return None;
        }
        if state
            .frames
            .last()
            .is_none_or(|f| f.pad != pad.as_ptr() as usize)
        {
            runtime.stats.local().mismatched_hooks.fetch_add(1, Relaxed);
            return None;
        }
        if state.frames.pop().unwrap().recording {
            state.sampled.pop()
        } else {
            None
        }
    });
    if let Some(mut record) = record {
        record.end = ts;
        record.flow_return = flow_return;
        runtime.submit(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{
        tests::{runtime, Collector},
        Config,
    };
    use std::time::Duration;

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
