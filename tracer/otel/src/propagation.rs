//! Owned trace tokens in buffer metadata. Metadata transformations clone ownership.
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use glib::translate::IntoGlib;
use gstreamer::{self as gst};

use crate::capture::Token;
use crate::export::Counters;

#[repr(C)]
struct ContextMeta {
    meta: gst::ffi::GstMeta,
    token: std::mem::ManuallyDrop<Token>,
}

fn api() -> glib::ffi::GType {
    static API: OnceLock<glib::ffi::GType> = OnceLock::new();
    *API.get_or_init(|| unsafe {
        let mut tags = [std::ptr::null()];
        gst::ffi::gst_meta_api_type_register(c"GstOtelContextV2API".as_ptr(), tags.as_mut_ptr())
    })
}
fn info() -> *const gst::ffi::GstMetaInfo {
    // GstMetaInfo is registered once and lives for the process lifetime.
    static INFO: OnceLock<usize> = OnceLock::new();
    *INFO.get_or_init(|| unsafe {
        gst::ffi::gst_meta_register(
            api(),
            c"GstOtelContextV2".as_ptr(),
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
        std::ptr::addr_of_mut!((*(meta as *mut ContextMeta)).token),
        std::mem::ManuallyDrop::new(token),
    );
    glib::ffi::GTRUE
}
unsafe extern "C" fn free(meta: *mut gst::ffi::GstMeta, _: *mut gst::ffi::GstBuffer) {
    std::mem::ManuallyDrop::drop(&mut (*(meta as *mut ContextMeta)).token);
}
unsafe extern "C" fn transform(
    dest: *mut gst::ffi::GstBuffer,
    meta: *mut gst::ffi::GstMeta,
    source: *mut gst::ffi::GstBuffer,
    _: glib::ffi::GQuark,
    _: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    let token =
        read_raw(source).unwrap_or_else(|| Token::clone(&(*(meta as *const ContextMeta)).token));
    // Avoid duplicating a context if an element combines metadata from inputs.
    // Multi-input causality needs element-specific links; retain the first input.
    if !gst::ffi::gst_buffer_get_meta(dest, api()).is_null() {
        return glib::ffi::GTRUE;
    }
    (!gst::ffi::gst_buffer_add_meta(dest, info(), &token as *const Token as *mut _).is_null())
        .into_glib()
}

// Shared buffers retain a common ancestor. Sidecar pointers are installed once
// and remain stable until buffer destruction, so existing buffers synchronize
// only with their own readers/writers. The global lock covers first installation.
static FALLBACK_QUARK: LazyLock<glib::Quark> =
    LazyLock::new(|| glib::Quark::from_str("gst.otel.fallback.v3"));
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

unsafe fn read_raw(buffer: *mut gst::ffi::GstBuffer) -> Option<Token> {
    // The caller's buffer reference keeps this permanently installed slot alive.
    let sidecar = gst::ffi::gst_mini_object_get_qdata(buffer.cast(), FALLBACK_QUARK.into_glib())
        as *const Fallback;
    if let Some(sidecar) = sidecar.as_ref() {
        if let Some(token) = &*sidecar.token.read().unwrap() {
            return Some(token.clone());
        }
    }
    let meta = gst::ffi::gst_buffer_get_meta(buffer, api()) as *const ContextMeta;
    (!meta.is_null()).then(|| Token::clone(&(*meta).token))
}

pub(crate) fn read(buffer: &gst::Buffer) -> Option<Token> {
    unsafe { read_raw(buffer.as_ptr() as *mut _) }
}

pub(crate) fn write(buffer: &gst::Buffer, token: &Token, is_root: bool, counters: &Counters) {
    unsafe {
        let ptr = buffer.as_ptr() as *mut gst::ffi::GstBuffer;
        let mini = ptr.cast();
        if gst::ffi::gst_mini_object_is_writable(mini) != glib::ffi::GFALSE {
            if let Some(sidecar) = fallback(buffer, false) {
                *sidecar.token.write().unwrap() = None;
            }
            let meta = gst::ffi::gst_buffer_get_meta(ptr, api()) as *mut ContextMeta;
            if meta.is_null() {
                if gst::ffi::gst_buffer_add_meta(ptr, info(), token as *const Token as *mut _)
                    .is_null()
                {
                    counters.local().propagation_misses.fetch_add(1, Relaxed);
                }
            } else {
                // No outstanding borrowed token escapes read(); buffer writability
                // excludes concurrent access to its metadata.
                *(*meta).token = token.clone();
            }
        } else {
            counters.local().propagation_fallbacks.fetch_add(1, Relaxed);
            if !is_root && !gst::ffi::gst_buffer_get_meta(ptr, api()).is_null() {
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
