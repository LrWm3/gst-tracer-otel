//! Optional NVTX worker scopes. Bind domains during the owning element's start,
//! never by a process-wide UID or thread name. No CUDA dependency or span handoff.
use crate::profilecontext::{Description, Lease};
use gstreamer::{self as gst, prelude::*};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{CStr, CString},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, LazyLock, Mutex, Weak,
    },
};

const MAX_DOMAINS: usize = 4096;
const MAX_DEPTH: usize = 256;
static ENABLED: AtomicBool = AtomicBool::new(false);
static CREATED: AtomicU64 = AtomicU64::new(0);
static RANGES: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);
static DOMAINS: LazyLock<Mutex<Domains>> = LazyLock::new(Default::default);

#[derive(Default)]
struct Domains {
    next: usize,
    entries: HashMap<usize, Weak<Lease>>,
}
impl Domains {
    fn insert(&mut self, owner: &Arc<Lease>) -> usize {
        // Weak ownership also releases capacity when an element fails to destroy
        // its domain. IDs never repeat, including after an element is recreated.
        self.entries.retain(|_, owner| owner.strong_count() != 0);
        if self.entries.len() >= MAX_DOMAINS || self.next == usize::MAX {
            return 0;
        }
        self.next += 1;
        self.entries.insert(self.next, Arc::downgrade(owner));
        self.next
    }
}

#[derive(Default)]
struct Starts {
    stack: Vec<(usize, gst::StateChange, Description)>,
    overflow: usize,
}
thread_local! { static STARTS: RefCell<Starts> = RefCell::default(); }

/// Diagnostic counts for the optional worker bridge, independent of trace sampling.
#[derive(Debug, Default, Clone, Copy)]
pub struct NvtxProfileStats {
    pub bound_domains: u64,
    pub active_domains: usize,
    pub entered_ranges: u64,
    pub capacity_rejections: u64,
}
pub fn nvtx_profile_stats() -> NvtxProfileStats {
    NvtxProfileStats {
        bound_domains: CREATED.load(Relaxed),
        active_domains: DOMAINS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .values()
            .filter(|owner| owner.strong_count() != 0)
            .count(),
        entered_ranges: RANGES.load(Relaxed),
        capacity_rejections: REJECTED.load(Relaxed),
    }
}
pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

pub(crate) fn initialize() -> Result<(), String> {
    let Some(path) = std::env::var_os("MARS_PROFILE_NVTX_ADAPTER") else {
        return Ok(());
    };
    if !crate::profilecontext::enabled() {
        return Err("NVTX worker attribution requires MARS_PROFILE_ELEMENTS=true".into());
    }
    let path = std::fs::canonicalize(path).map_err(|e| format!("NVTX adapter: {e}"))?;
    let injected =
        std::env::var_os("NVTX_INJECTION64_PATH").and_then(|v| std::fs::canonicalize(v).ok());
    if injected.as_ref() != Some(&path) {
        return Err("set NVTX_INJECTION64_PATH to MARS_PROFILE_NVTX_ADAPTER before process startup; NVTX injection cannot be shared with another profiler".into());
    }
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| "NVTX path contains NUL")?;
    // SAFETY: explicit user-selected library, with our versioned C ABI. NVTX may
    // retain function pointers, so the DSO remains resident until process exit.
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let error = libc::dlerror();
            return Err(if error.is_null() {
                "cannot load NVTX adapter".into()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            });
        }
        let start = libc::dlsym(handle, c"mars_nvtx_start_v1".as_ptr());
        if start.is_null() {
            return Err("NVTX adapter ABI v1 symbol missing".into());
        }
        type Receive = unsafe extern "C" fn(u32, usize, *const libc::c_char) -> usize;
        let start: unsafe extern "C" fn(Receive) -> i32 = std::mem::transmute(start);
        if start(receive) != 0 {
            return Err("NVTX adapter already initialized or initialization failed".into());
        }
    }
    ENABLED.store(true, Relaxed);
    Ok(())
}

pub(crate) fn state_pre(element: &gst::Element, change: gst::StateChange) {
    if !enabled() || !crate::profiles::active() {
        return;
    }
    let owner = (change == gst::StateChange::ReadyToPaused
        && element.factory().is_some_and(|f| f.name() == "nvinfer"))
    .then(|| crate::metadata::profile_id(element.upcast_ref()))
    .flatten();
    STARTS.with(|starts| {
        let mut starts = starts.borrow_mut();
        if starts.overflow != 0 || starts.stack.len() == MAX_DEPTH {
            starts.overflow += 1;
        } else {
            starts
                .stack
                .push((element.as_ptr() as usize, change, owner));
        }
    });
}
pub(crate) fn state_post(element: &gst::Element, change: gst::StateChange) {
    if !enabled() {
        return;
    }
    let _ = STARTS.try_with(|starts| {
        if let Ok(mut starts) = starts.try_borrow_mut() {
            if starts.overflow != 0 {
                starts.overflow -= 1;
            } else if starts
                .stack
                .last()
                .is_some_and(|(e, c, _)| *e == element.as_ptr() as usize && *c == change)
            {
                starts.stack.pop();
            } else {
                starts.stack.clear();
            }
        }
    });
}

unsafe extern "C" fn receive(kind: u32, domain: usize, name: *const libc::c_char) -> usize {
    if !enabled() {
        return 0;
    }
    match kind {
        1 => {
            if name.is_null() || !crate::profiles::active() {
                return 0;
            }
            // SAFETY: adapter promises a valid NUL-terminated string. Inspect a
            // bounded prefix only; arbitrary messages/batch numbers are not keys.
            let name =
                unsafe { std::slice::from_raw_parts(name.cast::<u8>(), libc::strnlen(name, 64)) };
            if !name.strip_prefix(b"GstNvInfer: UID=").is_some_and(|id| {
                !id.is_empty() && id.len() <= 10 && id.iter().all(u8::is_ascii_digit)
            }) {
                return 0;
            }
            let owner = STARTS
                .try_with(|starts| {
                    let starts = starts.try_borrow().ok()?;
                    if starts.overflow != 0 {
                        return None;
                    }
                    starts.stack.last()?.2.clone()
                })
                .ok()
                .flatten();
            let Some(owner) = owner else { return 0 };
            let id = DOMAINS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(&owner);
            if id == 0 {
                REJECTED.fetch_add(1, Relaxed);
            } else {
                CREATED.fetch_add(1, Relaxed);
            }
            id
        }
        2 => {
            DOMAINS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .remove(&domain);
            0
        }
        3 if domain != 0 && crate::profiles::active() => {
            let owner = DOMAINS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .get(&domain)
                .and_then(Weak::upgrade);
            // A retired/destroyed domain is explicitly unknown, so it cannot
            // silently inherit a surrounding element's work on a reused worker.
            RANGES.fetch_add(1, Relaxed);
            crate::profilecontext::worker_enter(domain, owner)
        }
        4 if domain != 0 => crate::profilecontext::worker_leave(domain),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_bounded_and_do_not_keep_elements_alive_or_reuse_ids() {
        let mut domains = Domains::default();
        let owner = crate::profilecontext::intern("nvtx-registry-test", "nvinfer").unwrap();
        for expected in 1..=MAX_DOMAINS {
            assert_eq!(domains.insert(&owner), expected);
        }
        assert_eq!(domains.insert(&owner), 0);
        drop(owner);
        assert!(domains
            .entries
            .values()
            .all(|owner| owner.upgrade().is_none()));
        let owner = crate::profilecontext::intern("nvtx-registry-next", "nvinfer").unwrap();
        assert_eq!(domains.insert(&owner), MAX_DOMAINS + 1);
        assert_eq!(domains.entries.len(), 1);
    }
}
