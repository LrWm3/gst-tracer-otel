//! Weak, object-local dependencies for immutable metric snapshots.
//! Cache hits only read their validity flag; graph hooks and cache construction
//! perform the qdata and watcher-list work without retaining GStreamer objects.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, LazyLock, Mutex, Weak,
};

use glib::translate::IntoGlib;
use gstreamer::{self as gst, prelude::*};

pub(crate) struct Validity(AtomicBool);
impl Default for Validity {
    fn default() -> Self {
        Self(AtomicBool::new(true))
    }
}
impl Validity {
    pub fn is_valid(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn invalidate(&self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct Watchers(Mutex<Vec<Weak<Validity>>>);
impl Watchers {
    fn invalidate(&self) {
        // Invalid snapshots remain usable by in-flight calls, but must never
        // be reused by a later lookup, including after pad-address reuse.
        for snapshot in self.0.lock().unwrap().drain(..).filter_map(|v| v.upgrade()) {
            snapshot.invalidate();
        }
    }
}
impl Drop for Watchers {
    fn drop(&mut self) {
        self.invalidate();
    }
}

static WATCHERS: LazyLock<glib::ffi::GQuark> =
    LazyLock::new(|| glib::Quark::from_str("mars-metrics.cache-watchers").into_glib());

unsafe extern "C" fn drop_watchers(ptr: glib::ffi::gpointer) {
    drop(Box::from_raw(ptr.cast::<Watchers>()));
}

fn watch(object: &gst::Object, validity: &Arc<Validity>) {
    // This private qdata is installed once and never replaced. The caller's
    // object reference protects it until the watcher registration finishes.
    let watchers = unsafe {
        let object = object.as_ptr().cast();
        let mut ptr = glib::gobject_ffi::g_object_get_qdata(object, *WATCHERS).cast::<Watchers>();
        if ptr.is_null() {
            let candidate = Box::into_raw(Box::<Watchers>::default());
            if glib::gobject_ffi::g_object_replace_qdata(
                object,
                *WATCHERS,
                std::ptr::null_mut(),
                candidate.cast(),
                Some(drop_watchers),
                std::ptr::null_mut(),
            ) != glib::ffi::GFALSE
            {
                ptr = candidate;
            } else {
                drop(Box::from_raw(candidate));
                ptr = glib::gobject_ffi::g_object_get_qdata(object, *WATCHERS).cast();
            }
        }
        &*ptr
    };
    let mut snapshots = watchers.0.lock().unwrap();
    snapshots.retain(|weak| weak.upgrade().is_some_and(|v| v.is_valid()));
    let weak = Arc::downgrade(validity);
    if !snapshots.iter().any(|entry| entry.ptr_eq(&weak)) {
        snapshots.push(weak);
    }
}

pub(crate) fn watch_path(object: &gst::Object, validity: &Arc<Validity>) {
    watch(object, validity);
    let mut parent = object.parent();
    while let Some(object) = parent {
        watch(&object, validity);
        parent = object.parent();
    }
}

pub(crate) fn invalidate(object: &gst::Object) {
    unsafe {
        let ptr = glib::gobject_ffi::g_object_get_qdata(object.as_ptr().cast(), *WATCHERS)
            .cast::<Watchers>();
        if let Some(watchers) = ptr.as_ref() {
            watchers.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_rebuilds_and_invalidations_reclaim_weak_subscriptions() {
        gst::init().unwrap();
        let object = gst::Bin::new();
        let mut retained = Vec::new();
        for _ in 0..1000 {
            let validity = Arc::new(Validity::default());
            watch_path(object.upcast_ref(), &validity);
            watch_path(object.upcast_ref(), &validity);
            let watchers = unsafe {
                &*glib::gobject_ffi::g_object_get_qdata(object.as_ptr().cast(), *WATCHERS)
                    .cast::<Watchers>()
            };
            assert_eq!(watchers.0.lock().unwrap().len(), 1);
            validity.invalidate();
            // Retaining invalid snapshots must not retain their subscriptions.
            retained.push(validity);
        }
        invalidate(object.upcast_ref());
        let validity = Arc::new(Validity::default());
        watch_path(object.upcast_ref(), &validity);
        let weak = object.downgrade();
        drop(object);
        assert!(weak.upgrade().is_none());
        assert!(!validity.is_valid());
    }
}
