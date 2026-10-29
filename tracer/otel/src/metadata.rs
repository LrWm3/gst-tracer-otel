//! Cached descriptions with weak, object-local invalidation subscriptions.
use arc_swap::ArcSwapOption;
use glib::translate::IntoGlib;
use gstreamer::{self as gst, prelude::*};
use opentelemetry::KeyValue;
use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc, LazyLock, Mutex, OnceLock, Weak,
};

use crate::export::text_value;

pub(crate) struct Site {
    pub span_names: [Arc<str>; 3],
    pub attributes: Vec<KeyValue>,
    pub metrics: OnceLock<Arc<crate::metrics::Series>>,
    pub metric_rejected_epoch: AtomicU64,
    pub encoded: OnceLock<crate::wire::EncodedAttributes>,
    pub(crate) epoch: u64,
}
impl Site {
    pub fn span_name(&self, operation: crate::capture::Operation) -> &str {
        let name = &self.span_names[operation as usize];
        if name.is_empty() {
            operation.name()
        } else {
            name
        }
    }
}
struct PadState {
    epoch: AtomicU64,
    // Generation in the high bits, source classification in bit zero.
    source: AtomicU64,
    root_gate: ArcSwapOption<RootGate>,
    site: ArcSwapOption<Site>,
    #[cfg(feature = "profiles")]
    profile: ArcSwapOption<ProfileSite>,
}
pub(crate) struct RootGate {
    owner: Weak<()>,
    next_ns: AtomicU64,
}
impl RootGate {
    pub fn ready(&self, ts: u64) -> bool {
        ts >= self.next_ns.load(Relaxed)
    }

    pub fn admit(&self, ts: u64, period_ns: u64) -> bool {
        let mut next = self.next_ns.load(Relaxed);
        loop {
            if ts < next {
                return false;
            }
            match self.next_ns.compare_exchange_weak(
                next,
                ts.saturating_add(period_ns),
                Relaxed,
                Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => next = actual,
            }
        }
    }
}
#[cfg(feature = "profiles")]
struct ProfileSite {
    epoch: u64,
    capacity_epoch: u64,
    id: crate::profilecontext::Description,
}
impl PadState {
    fn invalidate(&self) {
        self.epoch.fetch_add(1, Relaxed);
    }
}
#[derive(Default)]
struct Watchers {
    pads: Mutex<Vec<Weak<PadState>>>,
}
impl Watchers {
    fn invalidate(&self) {
        self.pads.lock().unwrap().retain(|weak| {
            if let Some(pad) = weak.upgrade() {
                pad.invalidate();
                true
            } else {
                false
            }
        });
    }
}
impl Drop for Watchers {
    fn drop(&mut self) {
        self.invalidate();
    }
}
static STATE: LazyLock<glib::Quark> = LazyLock::new(|| glib::Quark::from_str("gst.otel.pad.v3"));
static WATCHERS: LazyLock<glib::Quark> =
    LazyLock::new(|| glib::Quark::from_str("gst.otel.watchers.v3"));
// Only first-use and descriptor rebuilds enter this lock, never cached reads.
static SETUP: Mutex<()> = Mutex::new(());
unsafe extern "C" fn drop_state(ptr: glib::ffi::gpointer) {
    drop(Box::from_raw(ptr as *mut Arc<PadState>));
}
unsafe extern "C" fn drop_watchers(ptr: glib::ffi::gpointer) {
    drop(Box::from_raw(ptr as *mut Arc<Watchers>));
}

fn state(object: &impl IsA<gst::Object>) -> &Arc<PadState> {
    // Install once. The borrowed object protects qdata lifetime, and no strong
    // reference is retained by a descriptor or invalidation subscription.
    unsafe {
        let object = object.as_ref().as_ptr().cast();
        let quark = STATE.into_glib();
        let mut ptr = glib::gobject_ffi::g_object_get_qdata(object, quark) as *mut Arc<PadState>;
        if ptr.is_null() {
            let candidate = Box::into_raw(Box::new(Arc::new(PadState {
                epoch: AtomicU64::new(1),
                source: AtomicU64::new(0),
                root_gate: ArcSwapOption::empty(),
                site: ArcSwapOption::empty(),
                #[cfg(feature = "profiles")]
                profile: ArcSwapOption::empty(),
            })));
            if glib::gobject_ffi::g_object_replace_qdata(
                object,
                quark,
                std::ptr::null_mut(),
                candidate.cast(),
                Some(drop_state),
                std::ptr::null_mut(),
            ) != glib::ffi::GFALSE
            {
                ptr = candidate;
            } else {
                drop(Box::from_raw(candidate));
                ptr = glib::gobject_ffi::g_object_get_qdata(object, quark).cast();
            }
        }
        &*ptr
    }
}

/// Gate state is owned by the pad and scoped to a runtime; it never retains
/// the pad or runtime. Atomic installation handles concurrent first use.
pub(crate) fn root_gate(pad: &gst::Pad, owner: &Arc<()>) -> Arc<RootGate> {
    let slot = &state(pad).root_gate;
    loop {
        let current = slot.load_full();
        if let Some(gate) = &current {
            if std::ptr::eq(gate.owner.as_ptr(), Arc::as_ptr(owner)) {
                return gate.clone();
            }
        }
        let gate = Arc::new(RootGate {
            owner: Arc::downgrade(owner),
            next_ns: AtomicU64::new(0),
        });
        let previous = slot.compare_and_swap(&current, Some(gate.clone()));
        if match (&current, &*previous) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        } {
            return gate;
        }
    }
}

/// Immutable, bounded profile descriptions. Cache hits avoid name/path lookups
/// and registry locks. Old IDs remain valid for queued CPU samples.
#[cfg(feature = "profiles")]
pub(crate) fn profile_id(object: &gst::Object) -> crate::profilecontext::Description {
    let state = state(object);
    let epoch = state.epoch.load(Relaxed);
    if let Some(site) = state.profile.load_full().filter(|s| {
        s.epoch == epoch
            && (s.id.is_some() || s.capacity_epoch == crate::profilecontext::capacity_epoch())
    }) {
        return site.id.clone();
    }
    watch_path(object, state);
    let element = object.downcast_ref::<gst::Element>().cloned().or_else(|| {
        object
            .downcast_ref::<gst::Pad>()
            .and_then(|p| p.parent_element())
    });
    let capacity_epoch = crate::profilecontext::capacity_epoch();
    let id = element.and_then(|element| {
        let path = text_value(&element.path_string(), 512);
        let factory = element.factory().map_or_else(
            || element.type_().name().to_owned(),
            |f| f.name().to_string(),
        );
        crate::profilecontext::intern(&path, &text_value(&factory, 128))
    });
    state.profile.store(Some(Arc::new(ProfileSite {
        epoch,
        capacity_epoch,
        id: id.clone(),
    })));
    id
}

fn watch(object: &gst::Object, state: &Arc<PadState>) {
    let _setup = SETUP.lock().unwrap();
    unsafe {
        let ptr = object.as_ptr().cast();
        let quark = WATCHERS.into_glib();
        let mut data = glib::gobject_ffi::g_object_get_qdata(ptr, quark) as *mut Arc<Watchers>;
        if data.is_null() {
            let watchers = Arc::new(Watchers::default());
            for property in ["name", "parent"] {
                let listeners = watchers.clone();
                object.connect_notify(Some(property), move |_, _| listeners.invalidate());
            }
            data = Box::into_raw(Box::new(watchers));
            glib::gobject_ffi::g_object_set_qdata_full(
                ptr,
                quark,
                data.cast(),
                Some(drop_watchers),
            );
        }
        let mut pads = (&*data).pads.lock().unwrap();
        pads.retain(|weak| weak.strong_count() > 0);
        let weak = Arc::downgrade(state);
        if !pads.iter().any(|p| p.ptr_eq(&weak)) {
            pads.push(weak);
        }
    }
}

fn watch_path(object: &gst::Object, state: &Arc<PadState>) {
    watch(object, state);
    let mut parent = object.parent();
    while let Some(object) = parent {
        watch(&object, state);
        parent = object.parent();
    }
}

pub(crate) fn invalidate(object: &impl IsA<gst::Object>) {
    unsafe {
        let ptr = object.as_ref().as_ptr().cast();
        let pad =
            glib::gobject_ffi::g_object_get_qdata(ptr, STATE.into_glib()) as *const Arc<PadState>;
        if !pad.is_null() {
            (*pad).invalidate();
        }
        let watchers = glib::gobject_ffi::g_object_get_qdata(ptr, WATCHERS.into_glib())
            as *const Arc<Watchers>;
        if !watchers.is_null() {
            (*watchers).invalidate();
        }
    }
}

pub(crate) fn is_source(pad: &gst::Pad) -> bool {
    let state = state(pad);
    let epoch = state.epoch.load(Relaxed);
    let cached = state.source.load(Relaxed);
    if cached >> 1 == epoch {
        return cached & 1 != 0;
    }
    watch(pad.upcast_ref(), state);
    let parent = pad.parent_element();
    if let Some(parent) = &parent {
        watch(parent.upcast_ref(), state);
    }
    let source = parent.is_some_and(|p| p.element_flags().contains(gst::ElementFlags::SOURCE));
    state
        .source
        .store((epoch << 1) | u64::from(source), Relaxed);
    source
}

pub(crate) fn site(pad: &gst::Pad, limit: usize) -> Arc<Site> {
    let state = state(pad);
    let epoch = state.epoch.load(Relaxed);
    if let Some(site) = state.site.load_full().filter(|s| s.epoch == epoch) {
        return site;
    }
    watch_path(pad.upcast_ref(), state);
    let peer = pad.peer();
    if let Some(peer) = &peer {
        watch_path(peer.upcast_ref(), state);
    }
    // Pull hooks receive the requesting sink pad. Attribute names always follow
    // buffer flow, so its upstream peer is still src_pad.
    let (src, sink) = if pad.direction() == gst::PadDirection::Sink {
        (peer.as_ref(), Some(pad))
    } else {
        (Some(pad), peer.as_ref())
    };
    let factory = |pad: Option<&gst::Pad>| {
        pad.and_then(|p| p.parent_element())
            .and_then(|e| e.factory())
            .map(|f| text_value(f.name().as_str(), 128).to_string())
            .unwrap_or_default()
    };
    let source_factory = factory(src);
    let sink_factory = factory(sink);
    let span_name = |operation: crate::capture::Operation, factory: &str| -> Arc<str> {
        if factory.is_empty() {
            operation.name().into()
        } else {
            format!("{} {factory}", operation.name()).into()
        }
    };
    let span_names = [
        span_name(crate::capture::Operation::Push, &sink_factory),
        span_name(crate::capture::Operation::Pull, &source_factory),
        span_name(crate::capture::Operation::PushList, &sink_factory),
    ];
    let values = [
        ("src_pad.factory", source_factory),
        ("sink_pad.factory", sink_factory),
        (
            "src_pad.element",
            src.and_then(|p| p.parent_element())
                .map(|p| p.name().to_string())
                .unwrap_or_default(),
        ),
        (
            "src_pad.name",
            src.map(|p| p.name().to_string()).unwrap_or_default(),
        ),
        (
            "src_pad.path",
            src.map(|p| p.path_string().to_string()).unwrap_or_default(),
        ),
        (
            "sink_pad.element",
            sink.and_then(|p| p.parent_element())
                .map(|p| p.name().to_string())
                .unwrap_or_default(),
        ),
        (
            "sink_pad.name",
            sink.map(|p| p.name().to_string()).unwrap_or_default(),
        ),
        (
            "sink_pad.path",
            sink.map(|p| p.path_string().to_string())
                .unwrap_or_default(),
        ),
    ];
    let descriptor = Arc::new(Site {
        span_names,
        encoded: OnceLock::new(),
        metrics: OnceLock::new(),
        metric_rejected_epoch: AtomicU64::new(0),
        attributes: values
            .into_iter()
            .map(|(key, value)| KeyValue::new(key, text_value(&value, limit)))
            .collect(),
        epoch,
    });
    state.site.store(Some(descriptor.clone()));
    descriptor
}

#[cfg(test)]
pub(crate) fn has_description(pad: &gst::Pad) -> bool {
    state(pad).site.load().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    mod factoryless {
        use gst::subclass::prelude::*;
        use gstreamer as gst;
        #[derive(Default)]
        pub struct Element;
        #[glib::object_subclass]
        impl ObjectSubclass for Element {
            const NAME: &'static str = "MarsFactorylessTestElement";
            type Type = super::Factoryless;
            type ParentType = gst::Element;
        }
        impl ObjectImpl for Element {}
        impl GstObjectImpl for Element {}
        impl ElementImpl for Element {}
    }
    glib::wrapper! {
        pub struct Factoryless(ObjectSubclass<factoryless::Element>) @extends gst::Element, gst::Object;
    }

    #[test]
    fn names_use_the_callee_factory_and_fall_back_without_a_factory() {
        use crate::capture::Operation::{Pull, Push};
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc")
            .name("unstable-instance-123")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("identity")
            .name("another-instance-456")
            .build()
            .unwrap();
        let src = source.static_pad("src").unwrap();
        assert_eq!(site(&src, 4096).span_name(Push), "gst.pad.push");
        source.link(&sink).unwrap();
        invalidate(&src);
        let descriptor = site(&src, 4096);
        assert_eq!(descriptor.span_name(Push), "gst.pad.push identity");
        let sink_pad = sink.static_pad("sink").unwrap();
        assert_eq!(
            site(&sink_pad, 4096).span_name(Pull),
            "gst.pad.pull_range fakesrc"
        );
        sink.set_property("name", "renamed");
        assert_eq!(site(&src, 4096).span_name(Push), descriptor.span_name(Push));
        source.unlink(&sink);
        let bin = glib::Object::new::<Factoryless>();
        assert!(bin.factory().is_none());
        let pad = gst::Pad::builder(gst::PadDirection::Sink)
            .name("sink")
            .build();
        bin.add_pad(&pad).unwrap();
        src.link(&pad).unwrap();
        invalidate(&src);
        assert_eq!(site(&src, 4096).span_name(Push), "gst.pad.push");
    }

    #[cfg(feature = "profiles")]
    #[test]
    fn profile_descriptions_survive_rename_without_retaining_elements() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::with_name("profile-old");
        let element = gst::ElementFactory::make("identity")
            .name("instance")
            .build()
            .unwrap();
        pipeline.add(&element).unwrap();
        let pad = element.static_pad("sink").unwrap();
        let refs = element.ref_count();
        let before = profile_id(pad.upcast_ref()).unwrap();
        pipeline.set_property("name", "profile-new");
        let after = profile_id(pad.upcast_ref()).unwrap();
        assert_ne!(before.id, after.id);
        assert!(crate::profilecontext::frame(before.id << 1).contains("profile-old"));
        assert!(crate::profilecontext::frame(after.id << 1).contains("profile-new"));
        assert_eq!(element.ref_count(), refs);
        let weak = element.downgrade();
        drop(pad);
        drop(element);
        drop(pipeline);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn renaming_an_ancestor_invalidates_its_sites_without_touching_other_pipelines() {
        gst::init().unwrap();
        let first = gst::Pipeline::with_name("first");
        let second = gst::Pipeline::with_name("second");
        let a = gst::ElementFactory::make("fakesrc").build().unwrap();
        let b = gst::ElementFactory::make("fakesrc").build().unwrap();
        first.add(&a).unwrap();
        second.add(&b).unwrap();
        let ap = a.static_pad("src").unwrap();
        let bp = b.static_pad("src").unwrap();
        let before = site(&ap, 4096);
        let unrelated = site(&bp, 4096);
        first.set_property("name", "renamed");
        let after = site(&ap, 4096);
        assert!(!Arc::ptr_eq(&before, &after));
        assert!(Arc::ptr_eq(&unrelated, &site(&bp, 4096)));
        assert!(after
            .attributes
            .iter()
            .any(|a| a.key.as_str() == "src_pad.path" && a.value.to_string().contains("renamed")));
        // An invalidated description already captured in a pending record survives.
        assert!(before
            .attributes
            .iter()
            .any(|a| a.key.as_str() == "src_pad.path" && a.value.to_string().contains("first")));
    }

    #[test]
    fn relinking_refreshes_the_peer_and_subscriptions_do_not_retain_pads() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let one = gst::ElementFactory::make("fakesink")
            .name("one")
            .build()
            .unwrap();
        let two = gst::ElementFactory::make("identity")
            .name("two")
            .build()
            .unwrap();
        let pad = source.static_pad("src").unwrap();
        let refs = pad.ref_count();
        source.link(&one).unwrap();
        let before = site(&pad, 4096);
        source.unlink(&one);
        source.link(&two).unwrap();
        invalidate(&pad);
        let after = site(&pad, 4096);
        assert_eq!(
            before.span_name(crate::capture::Operation::Push),
            "gst.pad.push fakesink"
        );
        assert_eq!(
            after.span_name(crate::capture::Operation::Push),
            "gst.pad.push identity"
        );
        assert_ne!(before.attributes, after.attributes);
        assert!(after
            .attributes
            .iter()
            .any(|a| a.key.as_str() == "sink_pad.element" && a.value.to_string() == "two"));
        assert_eq!(pad.ref_count(), refs);
        let weak = Arc::downgrade(state(&pad));
        drop(pad);
        drop(source);
        assert!(weak.upgrade().is_none());
    }
}
