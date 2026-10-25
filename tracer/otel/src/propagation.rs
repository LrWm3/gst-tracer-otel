//! Owned trace tokens in buffer metadata. Metadata transformations clone ownership.
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};

use glib::translate::IntoGlib;
use gstreamer::{self as gst};

use crate::capture::Token;
use crate::export::Counters;

// Bounded by input count, never by pipeline lifetime. The usual single-input
// buffer carries no allocation. Links are immutable once published in metadata.
pub(crate) const MAX_LINKS: usize = 8;
#[derive(Clone, Default)]
pub(crate) struct Links {
    pub tokens: Vec<Token>,
    pub dropped: u32,
}
impl Links {
    pub fn insert(&mut self, token: &Token, parent: &Token) {
        if token.trace_id == opentelemetry::trace::TraceId::INVALID
            || token.span_id == opentelemetry::trace::SpanId::INVALID
            || same_span(token, parent)
            || self.tokens.iter().any(|t| same_span(t, token))
        {
            return;
        }
        if self.tokens.len() < MAX_LINKS {
            self.tokens.push(token.clone());
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }
    pub fn extend(&mut self, input: &Input, parent: &Token) {
        self.insert(&input.token, parent);
        if let Some(links) = &input.links {
            for token in &links.tokens {
                self.insert(token, parent);
            }
            self.dropped = self.dropped.saturating_add(links.dropped);
        }
    }
    pub fn finish(self) -> Option<Arc<Self>> {
        (!self.tokens.is_empty() || self.dropped != 0).then(|| Arc::new(self))
    }
    pub fn sdk(&self) -> opentelemetry_sdk::trace::SpanLinks {
        let mut links = opentelemetry_sdk::trace::SpanLinks::default();
        links.links = self
            .tokens
            .iter()
            .map(|t| opentelemetry::trace::Link::new(t.context(), vec![], 0))
            .collect();
        links.dropped_count = self.dropped;
        links
    }
}
fn same_span(a: &Token, b: &Token) -> bool {
    a.trace_id == b.trace_id && a.span_id == b.span_id
}
#[derive(Clone)]
pub(crate) struct Input {
    pub token: Token,
    pub links: Option<Arc<Links>>,
}
impl Input {
    fn merge(&mut self, other: &Input) {
        let mut links = self.links.as_deref().cloned().unwrap_or_default();
        links.extend(other, &self.token);
        self.links = links.finish();
    }
}

#[repr(C)]
struct ContextMeta {
    meta: gst::ffi::GstMeta,
    input: std::mem::ManuallyDrop<Input>,
    // Shared roots may override the immutable common ancestor. Keep that rare
    // state with the meta so normal reads/writes never consult mini-object qdata.
    shared: std::mem::ManuallyDrop<OnceLock<Box<Fallback>>>,
}

fn api() -> glib::ffi::GType {
    static API: OnceLock<glib::ffi::GType> = OnceLock::new();
    *API.get_or_init(|| unsafe {
        let mut tags = [std::ptr::null()];
        gst::ffi::gst_meta_api_type_register(c"GstOtelContextV4API".as_ptr(), tags.as_mut_ptr())
    })
}
fn info() -> *const gst::ffi::GstMetaInfo {
    // GstMetaInfo is registered once and lives for the process lifetime.
    static INFO: OnceLock<usize> = OnceLock::new();
    *INFO.get_or_init(|| unsafe {
        gst::ffi::gst_meta_register(
            api(),
            c"GstOtelContextV4".as_ptr(),
            std::mem::size_of::<ContextMeta>(),
            Some(init),
            Some(free),
            Some(transform),
        ) as usize
    }) as *const gst::ffi::GstMetaInfo
}
unsafe extern "C" fn init(
    meta: *mut gst::ffi::GstMeta,
    params: glib::ffi::gpointer,
    _: *mut gst::ffi::GstBuffer,
) -> glib::ffi::gboolean {
    let token = if params.is_null() {
        Token::rejected()
    } else {
        (*(params as *const Token)).clone()
    };
    std::ptr::write(
        std::ptr::addr_of_mut!((*(meta as *mut ContextMeta)).input),
        std::mem::ManuallyDrop::new(Input { token, links: None }),
    );
    std::ptr::write(
        std::ptr::addr_of_mut!((*(meta as *mut ContextMeta)).shared),
        std::mem::ManuallyDrop::new(OnceLock::new()),
    );
    glib::ffi::GTRUE
}
unsafe extern "C" fn free(meta: *mut gst::ffi::GstMeta, _: *mut gst::ffi::GstBuffer) {
    std::mem::ManuallyDrop::drop(&mut (*(meta as *mut ContextMeta)).input);
    std::mem::ManuallyDrop::drop(&mut (*(meta as *mut ContextMeta)).shared);
}
unsafe extern "C" fn transform(
    dest: *mut gst::ffi::GstBuffer,
    meta: *mut gst::ffi::GstMeta,
    source: *mut gst::ffi::GstBuffer,
    _: glib::ffi::GQuark,
    _: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    let input =
        read_raw(source).unwrap_or_else(|| Input::clone(&(*(meta as *const ContextMeta)).input));
    let existing = gst::ffi::gst_buffer_get_meta(dest, api()) as *mut ContextMeta;
    if !existing.is_null() {
        // A metadata-copy destination is writable. Preserve the first parent,
        // adding bounded links for other inputs rather than another ContextMeta.
        let mut first = read_raw(dest).unwrap();
        first.merge(&input);
        let existing = &mut *existing;
        if let Some(shared) = existing.shared.get() {
            *shared.token.write().unwrap() = None;
        }
        *existing.input = first;
        return glib::ffi::GTRUE;
    }
    if !install_meta(dest, &input.token) {
        return glib::ffi::GFALSE;
    }
    let dest_meta = gst::ffi::gst_buffer_get_meta(dest, api()) as *mut ContextMeta;
    *(*dest_meta).input = input;
    glib::ffi::GTRUE
}

// Initially shared buffers without our meta still need a qdata sidecar. Pointers
// remain stable until buffer destruction. INSTALL_LOCK covers first installation;
// GStreamer's qdata getter also takes its own global lock on every lookup.
static FALLBACK_QUARK: LazyLock<glib::Quark> =
    LazyLock::new(|| glib::Quark::from_str("gst.otel.fallback.v4"));
static INSTALL_LOCK: Mutex<()> = Mutex::new(());
struct Fallback {
    token: RwLock<Option<Token>>,
}
unsafe extern "C" fn free_fallback(ptr: glib::ffi::gpointer) {
    drop(Box::from_raw(ptr as *mut Fallback));
}

unsafe fn fallback(buffer: &gst::Buffer, create: bool) -> Option<&Fallback> {
    let mini = buffer.as_ptr().cast_mut().cast();
    let quark = FALLBACK_QUARK.into_glib();
    let mut ptr = gst::ffi::gst_mini_object_get_qdata(mini, quark) as *mut Fallback;
    if ptr.is_null() && create {
        let _install = INSTALL_LOCK.lock().unwrap();
        ptr = gst::ffi::gst_mini_object_get_qdata(mini, quark).cast();
        if ptr.is_null() {
            ptr = Box::into_raw(Box::new(Fallback {
                token: RwLock::new(None),
            }));
            gst::ffi::gst_mini_object_set_qdata(mini, quark, ptr.cast(), Some(free_fallback));
        }
    }
    ptr.as_ref()
}

// The caller owns writable metadata access and has checked that no context meta
// is installed. This also applies to a metadata transform's destination buffer.
unsafe fn install_meta(buffer: *mut gst::ffi::GstBuffer, token: &Token) -> bool {
    // Retire an initially-shared override before installing a meta. Otherwise
    // removing that meta later could resurrect stale context.
    let sidecar = gst::ffi::gst_mini_object_get_qdata(buffer.cast(), FALLBACK_QUARK.into_glib())
        as *const Fallback;
    if let Some(sidecar) = sidecar.as_ref() {
        *sidecar.token.write().unwrap() = None;
    }
    !gst::ffi::gst_buffer_add_meta(buffer, info(), token as *const Token as *mut _).is_null()
}

unsafe fn read_raw(buffer: *mut gst::ffi::GstBuffer) -> Option<Input> {
    let meta = gst::ffi::gst_buffer_get_meta(buffer, api()) as *const ContextMeta;
    if !meta.is_null() {
        if let Some(shared) = (*meta).shared.get() {
            if let Some(token) = &*shared.token.read().unwrap() {
                return Some(Input {
                    token: token.clone(),
                    links: None,
                });
            }
        }
        return Some(Input::clone(&(*meta).input));
    }
    // The caller's buffer reference keeps this permanently installed slot alive.
    let sidecar = gst::ffi::gst_mini_object_get_qdata(buffer.cast(), FALLBACK_QUARK.into_glib())
        as *const Fallback;
    if let Some(sidecar) = sidecar.as_ref() {
        if let Some(token) = &*sidecar.token.read().unwrap() {
            return Some(Input {
                token: token.clone(),
                links: None,
            });
        }
    }
    None
}

pub(crate) fn read(buffer: &gst::Buffer) -> Option<Token> {
    read_input(buffer).map(|input| input.token)
}

pub(crate) fn read_input(buffer: &gst::BufferRef) -> Option<Input> {
    unsafe { read_raw(buffer.as_ptr() as *mut _) }
}

/// Explicit propagation for aggregators whose outputs do not copy input metadata.
/// The first known input supplies the parent/sampling decision; other inputs
/// become links (up to eight). Missing inputs never cause a fresh sampled root.
/// Returns the number of inputs with known context, including rejected contexts.
pub fn propagate_input_contexts(output: &mut gst::BufferRef, inputs: &[&gst::BufferRef]) -> usize {
    let mut merged: Option<Input> = None;
    let mut count = 0;
    for input in inputs.iter().filter_map(|buffer| read_input(buffer)) {
        count += 1;
        match &mut merged {
            Some(first) => first.merge(&input),
            first @ None => *first = Some(input),
        }
    }
    let input = merged.unwrap_or(Input {
        token: Token::rejected(),
        links: None,
    });
    unsafe {
        let ptr = output.as_mut_ptr();
        let mut meta = gst::ffi::gst_buffer_get_meta(ptr, api()) as *mut ContextMeta;
        if meta.is_null() {
            if !install_meta(ptr, &input.token) {
                return 0;
            }
            meta = gst::ffi::gst_buffer_get_meta(ptr, api()) as *mut ContextMeta;
        }
        if let Some(shared) = (*meta).shared.get() {
            *shared.token.write().unwrap() = None;
        }
        *(*meta).input = input;
    }
    count
}

pub(crate) fn write(buffer: &gst::Buffer, token: &Token, is_root: bool, counters: &Counters) {
    unsafe {
        let ptr = buffer.as_ptr() as *mut gst::ffi::GstBuffer;
        let mini = ptr.cast();
        let meta = gst::ffi::gst_buffer_get_meta(ptr, api()) as *mut ContextMeta;
        if gst::ffi::gst_mini_object_is_writable(mini) != glib::ffi::GFALSE {
            if meta.is_null() {
                if !install_meta(ptr, token) {
                    counters.local().propagation_misses.fetch_add(1, Relaxed);
                }
            } else {
                if let Some(shared) = (*meta).shared.get() {
                    *shared.token.write().unwrap() = None;
                }
                let input = &mut *(*meta).input;
                input.links = None;
                // No outstanding borrowed token escapes read(); buffer writability
                // excludes concurrent access to its metadata.
                // Rejected descendants carry the same decision and identity.
                // Compare the owned token, never just the recyclable buffer address.
                if is_root || token.sampled() || input.token != *token {
                    input.token = token.clone();
                }
            }
        } else {
            counters.local().propagation_fallbacks.fetch_add(1, Relaxed);
            if !meta.is_null() {
                if is_root {
                    // The metadata remains installed while this buffer is shared.
                    // Only OnceLock/RwLock fields are mutated through shared access.
                    let shared = (*meta).shared.get_or_init(|| {
                        Box::new(Fallback {
                            token: RwLock::new(None),
                        })
                    });
                    *shared.token.write().unwrap() = Some(token.clone());
                }
                return;
            }
            let sidecar = fallback(buffer, true).unwrap();
            let mut value = sidecar.token.write().unwrap();
            if value.is_none() || is_root {
                *value = Some(token.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanId, TraceFlags, TraceId, TraceState};
    use std::sync::Arc;

    fn token(id: u64) -> Token {
        Token {
            trace_id: TraceId::from(id as u128),
            span_id: SpanId::from(id),
            flags: TraceFlags::SAMPLED,
            is_remote: false,
            trace_state: Some(Arc::new(
                TraceState::from_key_value([("vendor", "state")]).unwrap(),
            )),
        }
    }

    #[test]
    fn fan_in_copy_and_explicit_api_preserve_one_parent_and_bounded_owned_links() {
        gst::init().unwrap();
        let stats = Counters::default();
        let inputs: Vec<_> = (1..=12)
            .map(|id| {
                let buffer = gst::Buffer::new();
                write(&buffer, &token(id), true, &stats);
                buffer
            })
            .collect();
        let mut output = gst::Buffer::new();
        for input in &inputs {
            input
                .copy_into(output.get_mut().unwrap(), gst::BufferCopyFlags::META, ..)
                .unwrap();
        }
        // Repeating a retained context does not add a duplicate or a drop.
        inputs[1]
            .copy_into(output.get_mut().unwrap(), gst::BufferCopyFlags::META, ..)
            .unwrap();
        let merged = read_input(&output).unwrap();
        assert_eq!(merged.token.span_id, SpanId::from(1));
        let links = merged.links.unwrap();
        assert_eq!(links.tokens.len(), MAX_LINKS);
        assert_eq!(links.dropped, 3);
        let refs: Vec<_> = inputs.iter().map(|b| b.as_ref()).collect();
        let mut explicit = gst::Buffer::new();
        assert_eq!(
            propagate_input_contexts(explicit.get_mut().unwrap(), &refs),
            12
        );
        assert_eq!(
            read_input(&explicit).unwrap().links.unwrap().tokens,
            links.tokens
        );
        let copy = output.copy_deep().unwrap();
        drop(inputs);
        drop(output);
        assert_eq!(
            read_input(&copy).unwrap().links.unwrap().tokens,
            links.tokens
        );
        let weak = Arc::downgrade(&links);
        drop(links);
        write(&copy, &token(99), false, &stats);
        assert!(read_input(&copy).unwrap().links.is_none());
        assert!(
            weak.upgrade().is_none(),
            "consuming the last meta releases its link set"
        );
        assert_eq!(
            propagate_input_contexts(explicit.get_mut().unwrap(), &[]),
            0
        );
        assert!(!read(&explicit).unwrap().sampled());
        assert!(read_input(&explicit).unwrap().links.is_none());
    }

    #[test]
    fn fan_in_keeps_rejection_and_shared_root_override_clears_old_links() {
        gst::init().unwrap();
        let stats = Counters::default();
        let rejected = gst::Buffer::new();
        write(&rejected, &Token::rejected(), true, &stats);
        let sampled = gst::Buffer::new();
        write(&sampled, &token(2), true, &stats);
        let mut output = gst::Buffer::new();
        propagate_input_contexts(output.get_mut().unwrap(), &[&rejected, &sampled]);
        assert!(!read(&output).unwrap().sampled());
        assert_eq!(read_input(&output).unwrap().links.unwrap().tokens.len(), 1);
        let shared = output.clone();
        write(&shared, &token(3), true, &stats);
        assert!(read_input(&output).unwrap().links.is_none());
        assert!(read_input(&output.copy()).unwrap().links.is_none());
        // Copying only memory deliberately strips our metadata.
        let mut stripped = gst::Buffer::new();
        output
            .copy_into(
                stripped.get_mut().unwrap(),
                gst::BufferCopyFlags::MEMORY,
                ..,
            )
            .unwrap();
        assert!(read(&stripped).is_none());
    }

    #[test]
    fn copied_metadata_owns_its_token_after_original_is_freed() {
        gst::init().unwrap();
        let buffer = gst::Buffer::with_size(32).unwrap();
        let token = token(1);
        write(&buffer, &token, true, &Counters::default());
        let copy = buffer.copy();
        let deep_copy = buffer.copy_deep().unwrap();
        drop(buffer);
        assert_eq!(read(&copy).unwrap().span_id, token.span_id);
        drop(copy);
        assert_eq!(
            read(&deep_copy).unwrap().trace_state.as_deref(),
            token.trace_state.as_deref()
        );
        drop(deep_copy);
        assert_eq!(Arc::strong_count(token.trace_state.as_ref().unwrap()), 1);
    }

    #[test]
    fn shared_branches_keep_common_ancestor_and_recycled_roots_override_it() {
        gst::init().unwrap();
        let buffer = gst::Buffer::new();
        let stats = Counters::default();
        write(&buffer, &token(1), true, &stats);
        let shared = buffer.clone();
        write(&shared, &token(2), false, &stats);
        assert_eq!(read(&shared).unwrap().span_id, SpanId::from(1));
        write(&shared, &token(3), true, &stats);
        assert_eq!(read(&shared).unwrap().span_id, SpanId::from(3));
        // A meta copy also observes the root's sidecar override.
        assert_eq!(read(&shared.copy()).unwrap().span_id, SpanId::from(3));
        drop(shared);
        write(&buffer, &token(4), true, &stats);
        assert_eq!(read(&buffer).unwrap().span_id, SpanId::from(4));
    }

    #[test]
    fn shared_buffer_without_meta_uses_safe_sidecar() {
        gst::init().unwrap();
        let buffer = gst::Buffer::new();
        let shared = buffer.clone();
        let stats = Counters::default();
        write(&buffer, &Token::rejected(), true, &stats);
        assert!(!read(&shared).unwrap().sampled());
        write(&shared, &token(1), false, &stats);
        assert!(!read(&buffer).unwrap().sampled());
        // Sidecars cannot survive a copy of a buffer that had no writable meta.
        assert!(read(&buffer.copy()).is_none());
        assert_eq!(stats.snapshot().propagation_fallbacks, 2);
    }

    #[test]
    fn installing_metadata_retires_the_initially_shared_override() {
        gst::init().unwrap();
        for copy in [false, true] {
            let mut buffer = gst::Buffer::new();
            let shared = buffer.clone();
            let stats = Counters::default();
            let old = token(1);
            let new = token(2);
            write(&buffer, &old, true, &stats);
            assert_eq!(read(&shared).unwrap(), old);
            drop(shared);
            if copy {
                let source = gst::Buffer::new();
                write(&source, &new, true, &stats);
                source
                    .copy_into(buffer.get_mut().unwrap(), gst::BufferCopyFlags::META, ..)
                    .unwrap();
            } else {
                write(&buffer, &new, true, &stats);
            }
            assert_eq!(read(&buffer).unwrap(), new);
            unsafe {
                assert!(fallback(&buffer, false)
                    .unwrap()
                    .token
                    .read()
                    .unwrap()
                    .is_none());
                let ptr = buffer.as_ptr().cast_mut();
                let meta = gst::ffi::gst_buffer_get_meta(ptr, api());
                assert!(!meta.is_null());
                assert_ne!(
                    gst::ffi::gst_buffer_remove_meta(ptr, meta),
                    glib::ffi::GFALSE
                );
            }
            assert!(read(&buffer).is_none());
            assert_eq!(Arc::strong_count(old.trace_state.as_ref().unwrap()), 1);
            assert_eq!(Arc::strong_count(new.trace_state.as_ref().unwrap()), 1);
        }
    }

    #[test]
    fn concurrent_shared_metadata_overrides_and_copies_keep_owned_tokens() {
        gst::init().unwrap();
        let buffer = gst::Buffer::new();
        let stats = Counters::default();
        let tokens: Vec<_> = (1..=8).map(token).collect();
        write(&buffer, &tokens[0], true, &stats);
        let barrier = std::sync::Barrier::new(tokens.len());
        std::thread::scope(|scope| {
            for token in &tokens {
                let buffer = buffer.clone();
                let stats = &stats;
                let barrier = &barrier;
                let tokens = &tokens;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..500 {
                        write(&buffer, token, true, stats);
                        assert!(tokens.contains(&read(&buffer).unwrap()));
                        let copy = buffer.copy();
                        let copied = read(&copy).unwrap();
                        assert!(tokens.contains(&copied));
                        // Later overrides cannot mutate the copy's owned context.
                        write(&buffer, token, true, stats);
                        assert_eq!(read(&copy).unwrap(), copied);
                    }
                });
            }
        });
        assert_eq!(stats.snapshot().propagation_fallbacks, 8_000);
        write(&buffer, &tokens[0], true, &stats);
        assert_eq!(read(&buffer).unwrap(), tokens[0]);
        drop(buffer);
        for token in tokens {
            assert_eq!(Arc::strong_count(token.trace_state.as_ref().unwrap()), 1);
        }
    }

    #[test]
    fn rejected_tokens_preserve_external_identity_across_reuse_and_copies() {
        gst::init().unwrap();
        let stats = Counters::default();
        let buffer = gst::Buffer::new();
        let mut rejected = token(1);
        rejected.flags = TraceFlags::default();
        rejected.is_remote = true;
        write(&buffer, &rejected, true, &stats);
        for _ in 0..100 {
            write(&buffer, &rejected, false, &stats);
        }
        assert_eq!(read(&buffer).unwrap(), rejected);
        let copy = buffer.copy();
        assert_eq!(read(&copy).unwrap(), rejected);
        rejected.trace_id = TraceId::from(2);
        write(&buffer, &rejected, true, &stats);
        assert_eq!(read(&buffer).unwrap(), rejected);
        assert_ne!(read(&copy).unwrap(), rejected);
        // A descendant with a different inherited identity still replaces it.
        rejected.span_id = SpanId::from(99);
        write(&copy, &rejected, false, &stats);
        assert_eq!(read(&copy).unwrap(), rejected);
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn concurrent_sidecar_installation_and_reuse_preserve_owned_tokens() {
        gst::init().unwrap();
        let buffer = gst::Buffer::new();
        let stats = Arc::new(Counters::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let buffer = buffer.clone();
                let stats = stats.clone();
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        write(&buffer, &Token::rejected(), false, &stats);
                        assert!(!read(&buffer).unwrap().sampled());
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let slot = unsafe { fallback(&buffer, false).unwrap() as *const _ };
        write(&buffer, &Token::rejected(), true, &stats);
        assert_eq!(
            unsafe { fallback(&buffer, false).unwrap() as *const _ },
            slot
        );
        assert!(!read(&buffer).unwrap().sampled());
    }
}
