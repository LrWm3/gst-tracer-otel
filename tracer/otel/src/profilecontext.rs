//! Element attribution, independent of trace sampling. Hooks publish only an
//! integer to the sampler; no GObject, registry or TLS access occurs in signals.
//! Each sample has one exclusive execution scope, or a separately marked task
//! owner when outside a chain call. Native stacks and trace IDs stay unchanged.
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        atomic::{
            AtomicBool, AtomicU64,
            Ordering::{Acquire, Relaxed, Release},
        },
        Arc, LazyLock, Mutex, Weak,
    },
};

const MAX_DESCRIPTIONS: usize = 4096;
const MAX_DEPTH: usize = 256;
static ENABLED: AtomicBool = AtomicBool::new(false);
static REJECTED: AtomicU64 = AtomicU64::new(0);
static CAPACITY_EPOCH: AtomicU64 = AtomicU64::new(0);
static RETAINED: AtomicU64 = AtomicU64::new(0);
static RETIRED: AtomicU64 = AtomicU64::new(0);

/// Normal-thread ownership only. Metadata and every published TLS scope hold
/// a lease; the signal handler still reads just its immutable integer ID.
#[derive(Debug)]
pub(crate) struct Lease {
    pub id: u64,
    retired: Arc<AtomicBool>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        // The target thread cleared its published ID before releasing its last
        // lease. Any signal that read it has finished its ring write by now.
        self.retired.store(true, Release);
    }
}
pub(crate) type Description = Option<Arc<Lease>>;
struct Entry {
    key: (String, String),
    lease: Weak<Lease>,
    retired: Arc<AtomicBool>,
    frames: [Arc<str>; 2],
}

#[derive(Default)]
struct Descriptions {
    ids: HashMap<(String, String), u64>,
    entries: HashMap<u64, Entry>,
    next: u64,
}
impl Descriptions {
    fn intern(&mut self, path: &str, factory: &str) -> Description {
        let key = (path.to_owned(), factory.to_owned());
        if let Some(lease) = self
            .ids
            .get(&key)
            .and_then(|id| self.entries.get(id))
            .and_then(|entry| entry.lease.upgrade())
        {
            return Some(lease);
        }
        if self.entries.len() == MAX_DESCRIPTIONS || self.next == u64::MAX >> 1 {
            return None;
        }
        self.next += 1;
        let id = self.next;
        let retired = Arc::new(AtomicBool::new(false));
        let lease = Arc::new(Lease {
            id,
            retired: retired.clone(),
        });
        let leaf = path.rsplit('/').next().unwrap_or(path);
        let name = leaf.split_once(':').map_or(leaf, |(_, name)| name);
        self.entries.insert(
            id,
            Entry {
                key: key.clone(),
                lease: Arc::downgrade(&lease),
                retired,
                frames: [
                    format!("[element] {name} [{factory}] — {path}").into(),
                    format!("[task owner] {name} [{factory}] — {path}").into(),
                ],
            },
        );
        self.ids.insert(key, id);
        Some(lease)
    }
    fn candidates(&self) -> Vec<u64> {
        self.entries
            .iter()
            .filter_map(|(&id, entry)| entry.retired.load(Acquire).then_some(id))
            .collect()
    }
    fn retire(&mut self, candidates: Vec<u64>) -> usize {
        let mut count = 0;
        for id in candidates {
            if let Some(entry) = self.entries.remove(&id) {
                // Recreating the same path receives a new ID, even while the
                // original description is awaiting its last sample drain.
                if self.ids.get(&entry.key) == Some(&id) {
                    self.ids.remove(&entry.key);
                }
                count += 1;
            }
        }
        count
    }
    fn frame(&self, key: u64) -> Arc<str> {
        self.entries.get(&(key >> 1)).map_or_else(
            || "[unattributed GStreamer CPU]".into(),
            |entry| entry.frames[(key & 1) as usize].clone(),
        )
    }
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

pub(crate) fn intern(path: &str, factory: &str) -> Description {
    let mut descriptions = DESCRIPTIONS.lock().unwrap();
    let lease = descriptions.intern(path, factory);
    if lease.is_none() {
        REJECTED.fetch_add(1, Relaxed);
    }
    RETAINED.store(descriptions.entries.len() as u64, Relaxed);
    lease
}

pub(crate) fn capacity_epoch() -> u64 {
    CAPACITY_EPOCH.load(Relaxed)
}
pub(crate) fn retained() -> u64 {
    RETAINED.load(Relaxed)
}
pub(crate) fn retired() -> u64 {
    RETIRED.load(Relaxed)
}

/// Sole report worker: snapshot BEFORE draining every sampler slot. Acquire on
/// the retired flag observes all preceding ring writes for these IDs. IDs never
/// revive: a new lease for the same path always receives a fresh integer.
pub(crate) fn before_drain() -> Vec<u64> {
    DESCRIPTIONS.lock().unwrap().candidates()
}
/// Call only AFTER encoding the drained samples into self-contained pprof.
pub(crate) fn after_report(candidates: Vec<u64>) {
    let mut descriptions = DESCRIPTIONS.lock().unwrap();
    let count = descriptions.retire(candidates) as u64;
    RETAINED.store(descriptions.entries.len() as u64, Relaxed);
    RETIRED.fetch_add(count, Relaxed);
    if count != 0 {
        CAPACITY_EPOCH.fetch_add(1, Relaxed);
    }
}

/// Reporting thread only; strings stay valid through the report's encoding.
pub(crate) fn frame(key: u64) -> Arc<str> {
    DESCRIPTIONS.lock().unwrap().frame(key)
}

#[derive(Default)]
struct State {
    // Pad/domain identity, operation (chain/list/pull/worker), description lease.
    calls: Vec<(usize, u8, Description)>,
    owner: Option<(usize, Description)>,
    overflow: usize,
    invalid: bool,
}
impl State {
    fn correlation_allowed(&self) -> bool {
        if self.invalid || self.overflow != 0 {
            return false;
        }
        let Some((_, operation, worker)) = self.calls.last() else {
            return true;
        };
        if *operation != 3 {
            return true;
        }
        // A worker scope establishes element ownership, not an async span
        // handoff. Keep a synchronous span only when it belongs to that element.
        worker.as_ref().is_some_and(|worker| {
            self.calls
                .iter()
                .rev()
                .find(|(_, op, _)| *op != 3)
                .and_then(|(_, _, id)| id.as_ref())
                .is_some_and(|id| id.id == worker.id)
        })
    }
    fn current(&self) -> u64 {
        if self.invalid || self.overflow != 0 {
            return 0;
        }
        if let Some((_, _, id)) = self.calls.last() {
            return id.as_ref().map_or(0, |id| id.id << 1);
        }
        self.owner
            .as_ref()
            .and_then(|(_, id)| id.as_ref())
            .map_or(0, |id| (id.id << 1) | 1)
    }
    fn enter(&mut self, pad: usize, operation: u8, id: Description) {
        if self.overflow != 0 || self.calls.len() == MAX_DEPTH {
            self.overflow += 1;
            // Unlike pad calls, NVTX domains may pop out of global order. Once
            // their identities no longer fit, suppress attribution until drain.
            if operation == 3 {
                self.invalid = true;
            }
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
            .is_some_and(|(p, op, _)| *p == pad && *op == operation)
        {
            self.calls.pop();
            if self.calls.is_empty() {
                self.invalid = false;
            }
        } else {
            self.invalid = true;
        }
    }
    #[cfg(target_os = "linux")]
    fn worker_leave(&mut self, domain: usize) -> usize {
        let depth = self
            .calls
            .iter()
            .filter(|(d, op, _)| *d == domain && *op == 3)
            .count();
        if self.overflow != 0 {
            self.overflow -= 1;
            self.invalid = true;
        } else if let Some(index) = self
            .calls
            .iter()
            .rposition(|(d, op, _)| *d == domain && *op == 3)
        {
            // NVTX stacks are per domain; ranges in different domains may
            // overlap without being globally nested. Active pad calls may not.
            if self.calls[index + 1..].iter().all(|(_, op, _)| *op == 3) {
                self.calls.remove(index);
                if self.calls.is_empty() {
                    self.invalid = false;
                }
            } else {
                self.invalid = true;
            }
        } else {
            self.invalid = true;
        }
        depth
    }
}
#[derive(Default)]
struct Local(RefCell<State>);
impl Drop for Local {
    fn drop(&mut self) {
        // Clear before State drops its leases. If sampler TLS was destroyed
        // first, Registration already cleared the slot and removed the timer.
        crate::profiles::publish_element(0, false);
    }
}
thread_local! { static LOCAL: Local = Local::default(); }

fn active() -> bool {
    enabled() && crate::profiles::active()
}

#[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
pub(crate) fn correlation_allowed() -> bool {
    LOCAL
        .try_with(|local| local.0.try_borrow().is_ok_and(|s| s.correlation_allowed()))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub(crate) fn worker_enter(domain: usize, owner: Description) -> usize {
    if !active() {
        return 0;
    }
    crate::profiles::register_thread();
    crate::profiles::publish_element(0, false);
    LOCAL
        .try_with(|local| {
            let Ok(mut state) = local.0.try_borrow_mut() else {
                return 0;
            };
            state.enter(domain, 3, owner);
            crate::profiles::publish_element(state.current(), state.correlation_allowed());
            state
                .calls
                .iter()
                .filter(|(d, op, _)| *d == domain && *op == 3)
                .count()
        })
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
pub(crate) fn worker_leave(domain: usize) -> usize {
    crate::profiles::publish_element(0, false);
    LOCAL
        .try_with(|local| {
            let Ok(mut state) = local.0.try_borrow_mut() else {
                return 0;
            };
            let depth = state.worker_leave(domain);
            crate::profiles::publish_element(state.current(), state.correlation_allowed());
            depth
        })
        .unwrap_or(0)
}
/// Retain ownership until asynchronous GPU activity has been encoded.
#[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
pub(crate) fn gpu_snapshot() -> Option<(u64, Arc<Lease>)> {
    LOCAL
        .try_with(|local| {
            let state = local.0.try_borrow().ok()?;
            let key = state.current();
            if key == 0 {
                return None;
            }
            let lease = state
                .calls
                .last()
                .and_then(|(_, _, id)| id.as_ref())
                .or_else(|| state.owner.as_ref().and_then(|(_, id)| id.as_ref()))?;
            Some((key, lease.clone()))
        })
        .ok()
        .flatten()
}

pub(crate) fn begin(pad: &gst::Pad, operation: u8) {
    if !active() {
        return;
    }
    crate::profiles::register_thread();
    crate::profiles::publish_element(0, false);
    // In pull mode the receiving sink invokes the upstream source's getrange.
    let id = if operation == 2 {
        pad.peer()
            .and_then(|p| crate::metadata::profile_id(p.upcast_ref()))
    } else {
        crate::metadata::profile_id(pad.upcast_ref())
    };
    LOCAL.with(|local| {
        if let Ok(mut state) = local.0.try_borrow_mut() {
            state.enter(pad.as_ptr() as usize, operation, id);
            crate::profiles::publish_element(state.current(), state.correlation_allowed());
        }
    });
}
pub(crate) fn end(pad: &gst::Pad, operation: u8) {
    if !active() {
        return;
    }
    crate::profiles::publish_element(0, false);
    let _ = LOCAL.try_with(|local| {
        if let Ok(mut state) = local.0.try_borrow_mut() {
            state.leave(pad.as_ptr() as usize, operation);
            crate::profiles::publish_element(state.current(), state.correlation_allowed());
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
    crate::profiles::publish_element(0, false);
    let id = (kind == gst::StreamStatusType::Enter)
        .then(|| crate::metadata::profile_id(owner.upcast_ref()));
    LOCAL.with(|local| {
        if let Ok(mut state) = local.0.try_borrow_mut() {
            if let Some(id) = id {
                *state = State {
                    owner: Some((owner.as_ptr() as usize, id)),
                    ..Default::default()
                };
            } else if state
                .owner
                .as_ref()
                .is_some_and(|(p, _)| *p == owner.as_ptr() as usize)
            {
                *state = State::default();
            }
            crate::profiles::publish_element(state.current(), state.correlation_allowed());
        }
    });
}

pub(crate) fn register(tracer: &impl TracerImpl) {
    if !crate::profiles::active() {
        return;
    }
    // Pull hooks are owned by the tracer (or shared metric collector), which
    // invokes attribution alongside trace capture without duplicate callbacks.
    if !enabled() {
        return;
    }
    for hook in [
        TracerHook::PadChainPre,
        TracerHook::PadChainPost,
        TracerHook::PadChainListPre,
        TracerHook::PadChainListPost,
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
    fn retirement_waits_for_a_drain_and_never_reuses_an_id() {
        let mut registry = Descriptions::default();
        let original = registry
            .intern("pipeline/identity:first", "identity")
            .unwrap();
        let original_id = original.id;
        let queued_key = original_id << 1;
        assert!(registry.candidates().is_empty());
        drop(original);
        let candidates = registry.candidates();
        let replacement = registry
            .intern("pipeline/identity:first", "identity")
            .unwrap();
        assert_ne!(replacement.id, original_id);
        // Encode the queued sample before retiring its description. Recreating
        // the same path must not move it to a new ID or remove the new key.
        let encoded = registry.frame(queued_key);
        assert!(encoded.contains("identity:first"));
        assert_eq!(registry.retire(candidates), 1);
        assert_eq!(&*registry.frame(queued_key), "[unattributed GStreamer CPU]");
        assert!(encoded.contains("identity:first"));
        assert_eq!(
            registry
                .intern("pipeline/identity:first", "identity")
                .unwrap()
                .id,
            replacement.id
        );
        let candidates = registry.candidates();
        drop(replacement); // released after this report's drain boundary
        assert_eq!(registry.retire(candidates), 0);
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.retire(registry.candidates()), 1);
    }

    #[test]
    fn repeated_lifetimes_reclaim_capacity_without_retiring_live_scopes() {
        let mut registry = Descriptions::default();
        let live = registry.intern("active", "identity").unwrap();
        let mut last_id = live.id;
        for cycle in 0..3 {
            for i in 1..MAX_DESCRIPTIONS {
                let entry = registry
                    .intern(&format!("{cycle}/{i}"), "identity")
                    .unwrap();
                assert!(entry.id > last_id);
                last_id = entry.id;
            }
            assert_eq!(registry.entries.len(), MAX_DESCRIPTIONS);
            assert!(registry.intern("overflow", "identity").is_none());
            assert_eq!(registry.retire(registry.candidates()), MAX_DESCRIPTIONS - 1);
            assert_eq!(registry.entries.len(), 1);
            assert!(registry.frame(live.id << 1).contains("active"));
        }
    }

    fn lease(id: u64) -> Description {
        (id != 0).then(|| {
            Arc::new(Lease {
                id,
                retired: Arc::new(AtomicBool::new(false)),
            })
        })
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn worker_scopes_preserve_only_matching_synchronous_correlation() {
        let mut state = State::default();
        state.enter(1, 0, lease(10));
        state.enter(100, 3, lease(10));
        assert_eq!(state.current(), 20);
        assert!(state.correlation_allowed());
        state.enter(200, 3, lease(20));
        assert_eq!(state.current(), 40);
        assert!(!state.correlation_allowed());
        // Pop a different domain while the newest one remains active.
        state.worker_leave(100);
        assert_eq!(state.current(), 40);
        assert!(!state.correlation_allowed());
        state.worker_leave(200);
        assert_eq!(state.current(), 20);
        assert!(state.correlation_allowed());
        state.leave(1, 0);
        state.enter(200, 3, lease(20));
        assert!(
            !state.correlation_allowed(),
            "ownership alone cannot restore a span"
        );
        state.enter(300, 3, None);
        assert_eq!(
            state.current(),
            0,
            "retired domain must not inherit its caller"
        );
        state.worker_leave(300);
        assert_eq!(state.current(), 40);
        state.worker_leave(200);
        assert_eq!(state.current(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn overlapping_worker_overflow_stays_unknown_until_drained() {
        let mut state = State::default();
        for _ in 0..MAX_DEPTH {
            state.enter(100, 3, lease(10));
        }
        state.enter(200, 3, lease(20));
        assert_eq!(state.current(), 0);
        for _ in 0..MAX_DEPTH {
            state.worker_leave(100);
            assert_eq!(state.current(), 0);
            assert!(!state.correlation_allowed());
        }
        state.worker_leave(200);
        assert_eq!(state.current(), 0);
        // Lost identities stay suppressed; no guess at recovery from the
        // ordering of pops in independent domains.
        state.enter(300, 3, lease(30));
        assert_eq!(state.current(), 0);
    }

    #[test]
    fn nested_calls_restore_ownership_and_overflow_does_not_misattribute() {
        let mut state = State {
            owner: Some((1, lease(10))),
            ..Default::default()
        };
        assert_eq!(state.current(), 21);
        state.enter(1, 0, lease(20));
        state.enter(2, 0, lease(30));
        assert_eq!(state.current(), 60);
        state.leave(2, 0);
        assert_eq!(state.current(), 40);
        for _ in 1..=MAX_DEPTH {
            state.enter(3, 0, lease(40));
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
        state.enter(4, 0, lease(0));
        assert_eq!(
            state.current(),
            0,
            "unknown element must not inherit its caller"
        );
    }
}
