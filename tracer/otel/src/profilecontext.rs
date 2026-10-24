//! Element attribution, independent of trace sampling. Hooks publish only an
//! integer to the sampler; no GObject, registry or TLS access occurs in signals.
//! Each sample has one exclusive execution scope, or a separately marked task
//! owner when outside a chain call. Native stacks and trace IDs stay unchanged.
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, LazyLock, Mutex,
    },
};

const MAX_DESCRIPTIONS: usize = 4096;
const MAX_DEPTH: usize = 256;
static ENABLED: AtomicBool = AtomicBool::new(false);
static REJECTED: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Descriptions {
    ids: HashMap<(String, String), u64>,
    frames: Vec<[Arc<str>; 2]>,
}
static DESCRIPTIONS: LazyLock<Mutex<Descriptions>> = LazyLock::new(Default::default);

pub(crate) fn configure() -> Result<(), String> {
    let setting = crate::profiles::setting("ELEMENTS")
        .map(|v| v.into_string().map_err(|_| "profile ELEMENTS is not UTF-8"))
        .transpose()?;
    let enabled = match setting.as_deref() {
        None | Some("true" | "1") => true,
        Some("false" | "0") => false,
        _ => return Err("GST_OTEL_PROFILE_ELEMENTS must be true or false".into()),
    };
    ENABLED.store(enabled, Relaxed);
    Ok(())
}
pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}
pub(crate) fn rejected() -> u64 {
    REJECTED.load(Relaxed)
}

pub(crate) fn intern(path: &str, factory: &str) -> u64 {
    let mut descriptions = DESCRIPTIONS.lock().unwrap();
    let key = (path.to_owned(), factory.to_owned());
    if let Some(id) = descriptions.ids.get(&key) {
        return *id;
    }
    if descriptions.frames.len() == MAX_DESCRIPTIONS {
        REJECTED.fetch_add(1, Relaxed);
        return 0;
    }
    let id = descriptions.frames.len() as u64 + 1;
    let leaf = path.rsplit('/').next().unwrap_or(path);
    let name = leaf.split_once(':').map_or(leaf, |(_, name)| name);
    descriptions.frames.push([
        format!("[element] {name} [{factory}] — {path}").into(),
        format!("[task owner] {name} [{factory}] — {path}").into(),
    ]);
    descriptions.ids.insert(key, id);
    id
}

/// Reporting thread only. IDs are never recycled, including after rename or
/// destruction, so queued samples cannot acquire another element's description.
pub(crate) fn frame(key: u64) -> Arc<str> {
    let id = key >> 1;
    if id != 0 {
        if let Some(frames) = DESCRIPTIONS.lock().unwrap().frames.get(id as usize - 1) {
            return frames[(key & 1) as usize].clone();
        }
    }
    "[unattributed GStreamer CPU]".into()
}

#[derive(Default)]
struct State {
    // Identity, operation (chain/list/pull), immutable description ID.
    calls: Vec<(usize, u8, u64)>,
    owner: Option<(usize, u64)>,
    overflow: usize,
    invalid: bool,
}
impl State {
    fn current(&self) -> u64 {
        if self.invalid || self.overflow != 0 {
            return 0;
        }
        if let Some((_, _, id)) = self.calls.last() {
            return id << 1;
        }
        self.owner
            .filter(|(_, id)| *id != 0)
            .map_or(0, |(_, id)| (id << 1) | 1)
    }
    fn enter(&mut self, pad: usize, operation: u8, id: u64) {
        if self.overflow != 0 || self.calls.len() == MAX_DEPTH {
            self.overflow += 1;
        } else {
            self.calls.push((pad, operation, id));
        }
    }
    fn leave(&mut self, pad: usize, operation: u8) {
        if self.overflow != 0 {
            self.overflow -= 1;
        } else if self
            .calls
            .last()
            .is_some_and(|&(p, op, _)| p == pad && op == operation)
        {
            self.calls.pop();
            if self.calls.is_empty() {
                self.invalid = false;
            }
        } else {
            self.invalid = true;
        }
    }
}
thread_local! { static LOCAL: RefCell<State> = RefCell::new(State::default()); }

fn active() -> bool {
    enabled() && crate::profiles::active()
}
pub(crate) fn begin(pad: &gst::Pad, operation: u8) {
    if !active() {
        return;
    }
    crate::profiles::register_thread();
    crate::profiles::publish_element(0);
    // In pull mode the receiving sink invokes the upstream source's getrange.
    let id = if operation == 2 {
        pad.peer()
            .map_or(0, |p| crate::metadata::profile_id(p.upcast_ref()))
    } else {
        crate::metadata::profile_id(pad.upcast_ref())
    };
    LOCAL.with(|local| {
        if let Ok(mut state) = local.try_borrow_mut() {
            state.enter(pad.as_ptr() as usize, operation, id);
            crate::profiles::publish_element(state.current());
        }
    });
}
pub(crate) fn end(pad: &gst::Pad, operation: u8) {
    if !active() {
        return;
    }
    crate::profiles::publish_element(0);
    let _ = LOCAL.try_with(|local| {
        if let Ok(mut state) = local.try_borrow_mut() {
            state.leave(pad.as_ptr() as usize, operation);
            crate::profiles::publish_element(state.current());
        }
    });
}

pub(crate) fn message(element: &gst::Element, message: &gst::Message) {
    if !active() {
        return;
    }
    let gst::MessageView::StreamStatus(status) = message.view() else {
        return;
    };
    let (kind, owner) = status.get();
    // Bins may forward the same message. Only the owning element's synchronous
    // post runs in the task's ENTER/LEAVE context and may change thread ownership.
    if owner != *element
        || !matches!(
            kind,
            gst::StreamStatusType::Enter | gst::StreamStatusType::Leave
        )
    {
        return;
    }
    crate::profiles::register_thread();
    crate::profiles::publish_element(0);
    let id = (kind == gst::StreamStatusType::Enter)
        .then(|| crate::metadata::profile_id(owner.upcast_ref()));
    LOCAL.with(|local| {
        if let Ok(mut state) = local.try_borrow_mut() {
            if let Some(id) = id {
                *state = State {
                    owner: Some((owner.as_ptr() as usize, id)),
                    ..Default::default()
                };
            } else if state
                .owner
                .is_some_and(|(p, _)| p == owner.as_ptr() as usize)
            {
                *state = State::default();
            }
            crate::profiles::publish_element(state.current());
        }
    });
}

pub(crate) fn register(tracer: &impl TracerImpl) {
    if !crate::profiles::active() {
        return;
    }
    // Retain profiles-only pull-thread registration when attribution is off.
    tracer.register_hook(TracerHook::PadPullRangePre);
    if !enabled() {
        return;
    }
    for hook in [
        TracerHook::PadChainPre,
        TracerHook::PadChainPost,
        TracerHook::PadChainListPre,
        TracerHook::PadChainListPost,
        TracerHook::PadPullRangePost,
        TracerHook::ElementPostMessagePre,
    ] {
        tracer.register_hook(hook);
    }
}

// Share the typed GStreamer callbacks between the three tracer factories.
macro_rules! methods {
    () => {
        fn pad_chain_pre(&self, _: u64, pad: &gst::Pad, _: &gst::Buffer) {
            crate::profilecontext::begin(pad, 0);
        }
        fn pad_chain_post(
            &self,
            _: u64,
            pad: &gst::Pad,
            _: Result<gst::FlowSuccess, gst::FlowError>,
        ) {
            crate::profilecontext::end(pad, 0);
        }
        fn pad_chain_list_pre(&self, _: u64, pad: &gst::Pad, _: &gst::BufferList) {
            crate::profilecontext::begin(pad, 1);
        }
        fn pad_chain_list_post(
            &self,
            _: u64,
            pad: &gst::Pad,
            _: Result<gst::FlowSuccess, gst::FlowError>,
        ) {
            crate::profilecontext::end(pad, 1);
        }
        fn pad_pull_range_pre(&self, _: u64, pad: &gst::Pad, _: u64, _: u32) {
            crate::profiles::register_thread();
            crate::profilecontext::begin(pad, 2);
        }
        fn pad_pull_range_post(
            &self,
            _: u64,
            pad: &gst::Pad,
            _: Result<&gst::Buffer, gst::FlowError>,
        ) {
            crate::profilecontext::end(pad, 2);
        }
        fn element_post_message_pre(&self, _: u64, element: &gst::Element, message: &gst::Message) {
            crate::profilecontext::message(element, message);
        }
    };
}
pub(crate) use methods;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_calls_restore_ownership_and_overflow_does_not_misattribute() {
        let mut state = State {
            owner: Some((1, 10)),
            ..Default::default()
        };
        assert_eq!(state.current(), 21);
        state.enter(1, 0, 20);
        state.enter(2, 0, 30);
        assert_eq!(state.current(), 60);
        state.leave(2, 0);
        assert_eq!(state.current(), 40);
        for _ in 1..=MAX_DEPTH {
            state.enter(3, 0, 40);
        }
        assert_eq!(state.current(), 0);
        for _ in 1..=MAX_DEPTH {
            state.leave(3, 0);
        }
        assert_eq!(state.current(), 40);
        state.leave(9, 0);
        assert_eq!(state.current(), 0);
        state.leave(1, 0);
        assert_eq!(state.current(), 21);
        state.enter(4, 0, 0);
        assert_eq!(
            state.current(),
            0,
            "unknown element must not inherit its caller"
        );
    }
}
