//! Worker attribution through the native NVTX injection ABI, without a GPU.
#![cfg(all(feature = "profiles", target_os = "linux"))]
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use prost::Message;
use pyroscope::encode::gen::{google::Profile, push::PushRequest};
use std::{
    collections::HashMap,
    io::Read,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        OnceLock,
    },
    time::{Duration, Instant},
};

struct Adapter {
    create: unsafe extern "C" fn(*const libc::c_char) -> usize,
    destroy: unsafe extern "C" fn(usize),
    push: unsafe extern "C" fn(usize) -> i32,
    pop: unsafe extern "C" fn(usize) -> i32,
    launch: unsafe extern "C" fn(),
}
static ADAPTER: OnceLock<Adapter> = OnceLock::new();

mod imp {
    use super::*;
    #[derive(Default)]
    pub struct Inference {
        pub domain: AtomicUsize,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for Inference {
        const NAME: &'static str = "MarsTestNvtxInference";
        type Type = super::Inference;
        type ParentType = gst::Element;
    }
    impl ObjectImpl for Inference {}
    impl GstObjectImpl for Inference {}
    impl ElementImpl for Inference {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
            Some(META.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Test inference",
                    "Testing",
                    "NVTX worker ownership",
                    "Mars",
                )
            }))
        }
        fn change_state(
            &self,
            change: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            let adapter = ADAPTER.get().unwrap();
            if change == gst::StateChange::ReadyToPaused {
                // Same UID in every element/lifetime: only the actual owner
                // at domain creation may disambiguate these domains.
                let domain = unsafe { (adapter.create)(c"GstNvInfer: UID=1".as_ptr()) };
                assert_ne!(domain, 0);
                self.domain.store(domain, Relaxed);
            }
            let result = self.parent_change_state(change);
            if change == gst::StateChange::PausedToReady {
                unsafe { (adapter.destroy)(self.domain.swap(0, Relaxed)) };
            }
            result
        }
    }
}
glib::wrapper! { pub struct Inference(ObjectSubclass<imp::Inference>) @extends gst::Element, gst::Object; }

#[test]
fn nvtx_workers_share_attribution_without_inventing_trace_context() {
    let path = std::env::temp_dir().join(format!("mars nvtx mock-{}.so", std::process::id()));
    assert!(Command::new("cc")
        .args([
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-shared",
            "-fPIC",
            "-pthread",
            "native/nvtx_adapter.c",
            "native/nvtx_mock.c",
            "native/gpu_mock.c",
            "-o"
        ])
        .arg(&path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap()
        .success());
    let mut modes = vec![
        "cpu",
        "cpu-params",
        "profiles-only",
        "otel",
        "missing-injection",
        "wrong-injection",
        "signals-off",
        "empty",
    ];
    if cfg!(feature = "gpu-profiles") {
        modes.extend(["gpu", "combined"]);
    }
    for mode in modes {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "nvtx_child", "--ignored", "--nocapture"])
            .env_clear()
            .env("NVTX_TEST_MODE", mode)
            .env("NVTX_TEST_ADAPTER", &path)
            .env(
                "MARS_PROFILE_NVTX_ADAPTER",
                "/removed-selector-is-ignored.so",
            )
            .env(
                "NVTX_INJECTION64_PATH",
                match mode {
                    "missing-injection" => std::path::Path::new(""),
                    "wrong-injection" => std::path::Path::new("/proc/self/exe"),
                    _ => path.as_path(),
                },
            )
            .env("GST_DEBUG", "mars-otel:3")
            .env("MARS_GPU_ADAPTER", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if mode.ends_with("injection") {
            assert!(String::from_utf8_lossy(&output.stderr).contains("set NVTX_INJECTION64_PATH to the profile-nvtx-adapter library before process startup"));
        }
    }
    std::fs::remove_file(path).unwrap();
}

fn burn_cpu() {
    // Thread CPU time avoids silently losing test coverage under scheduling load.
    fn cpu_ns() -> u64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
            0
        );
        time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
    }
    let start = cpu_ns();
    while cpu_ns() - start < 45_000_000 {
        std::hint::black_box((0u64..1000).sum::<u64>());
    }
}

#[test]
#[ignore]
fn nvtx_child() {
    let mode = std::env::var("NVTX_TEST_MODE").unwrap();
    let gpu = matches!(mode.as_str(), "gpu" | "combined");
    let cpu = mode != "gpu";
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &url);
    std::env::set_var("MARS_PROFILE_URL", &url);
    let sources = if gpu {
        if cpu {
            "cpu,gpu"
        } else {
            "gpu"
        }
    } else {
        "cpu"
    }
    .parse::<gstoteltracer::ProfileSources>()
    .unwrap();
    std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "functions");
    std::env::set_var("MARS_SIGNALS", "traces,profiles");
    if mode == "signals-off" {
        std::env::set_var("MARS_SIGNALS", "traces");
    }
    let (stop, stopping) = std::sync::mpsc::channel();
    let server_thread = std::thread::spawn(move || {
        let mut profiles = Vec::new();
        while stopping.try_recv().is_err() {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            if request.url().contains("PusherService") {
                let mut compressed = Vec::new();
                request.as_reader().read_to_end(&mut compressed).unwrap();
                let mut bytes = Vec::new();
                libflate::gzip::Decoder::new(compressed.as_slice())
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                for series in PushRequest::decode(bytes.as_slice()).unwrap().series {
                    let is_gpu = series
                        .labels
                        .iter()
                        .any(|l| l.name == "__name__" && l.value == "gpu_kernel");
                    for sample in series.samples {
                        profiles.push((
                            is_gpu,
                            Profile::decode(sample.raw_profile.as_slice()).unwrap(),
                        ));
                    }
                }
            }
            request.respond(tiny_http::Response::empty(200)).unwrap();
        }
        profiles
    });
    gst::init().unwrap();
    let adapter_path = std::env::var("NVTX_TEST_ADAPTER").unwrap();
    let tracer: gst::Tracer = if mode == "profiles-only" {
        glib::Object::builder::<gstoteltracer::ProfilesTracer>()
            .property("profile-sources", sources)
            .property("params", format!("profile-nvtx-adapter=\"{adapter_path}\""))
            .build()
            .upcast()
    } else if mode == "otel" {
        glib::Object::builder::<gstoteltracer::TelemetryTracer>()
            .property("profile-nvtx-adapter", &adapter_path)
            .build()
            .upcast()
    } else {
        let mut builder = glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("profile-sources", sources)
            .property(
                "profile-nvtx-adapter",
                if matches!(mode.as_str(), "cpu-params" | "signals-off") {
                    "/invalid-property-is-not-loaded.so"
                } else {
                    &adapter_path
                },
            );
        if matches!(mode.as_str(), "cpu-params" | "empty") {
            let value = if mode == "empty" { "" } else { &adapter_path };
            builder = builder.property(
                "params",
                format!("profile-nvtx-adapter=(string)\"{value}\""),
            );
        }
        builder.build().upcast()
    };
    if mode.ends_with("injection") {
        assert_eq!(
            gstoteltracer::stats(),
            Err(gstoteltracer::FlushError::NotInitialized)
        );
        assert_eq!(gstoteltracer::profile_stats().active_threads, 0);
        assert_eq!(gstoteltracer::nvtx_profile_stats().bound_domains, 0);
        stop.send(()).unwrap();
        assert!(server_thread.join().unwrap().is_empty());
        return;
    }
    gstoteltracer::stats().unwrap();
    assert_eq!(
        tracer
            .property::<Option<String>>("profile-nvtx-adapter")
            .as_deref(),
        match mode.as_str() {
            "empty" => None,
            "signals-off" => Some("/invalid-property-is-not-loaded.so"),
            _ => Some(adapter_path.as_str()),
        }
    );
    let path = std::ffi::CString::new(adapter_path).unwrap();
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
        assert!(!handle.is_null());
        macro_rules! symbol {
            ($name:expr, $ty:ty) => {{
                let ptr = libc::dlsym(handle, $name.as_ptr());
                assert!(!ptr.is_null());
                std::mem::transmute::<*mut libc::c_void, $ty>(ptr)
            }};
        }
        symbol!(c"mars_nvtx_mock_initialize", unsafe extern "C" fn())();
        ADAPTER
            .set(Adapter {
                create: symbol!(
                    c"mars_nvtx_mock_create",
                    unsafe extern "C" fn(*const libc::c_char) -> usize
                ),
                destroy: symbol!(c"mars_nvtx_mock_destroy", unsafe extern "C" fn(usize)),
                push: symbol!(c"mars_nvtx_mock_push", unsafe extern "C" fn(usize) -> i32),
                pop: symbol!(c"mars_nvtx_mock_pop", unsafe extern "C" fn(usize) -> i32),
                launch: symbol!(c"mars_gpu_mock_launch_v1", unsafe extern "C" fn()),
            })
            .ok()
            .unwrap();
    }
    gst::Element::register(None, "nvinfer", gst::Rank::NONE, Inference::static_type()).unwrap();
    let adapter = ADAPTER.get().unwrap();
    assert_eq!(
        unsafe { (adapter.create)(c"GstNvInfer: UID=1".as_ptr()) },
        0,
        "no owning state transition"
    );
    if matches!(mode.as_str(), "signals-off" | "empty") {
        std::thread::spawn(move || unsafe {
            (adapter.push)(0);
            burn_cpu();
            (adapter.pop)(0);
        })
        .join()
        .unwrap();
        assert_eq!(gstoteltracer::profile_stats().active_threads, 0);
        assert_eq!(gstoteltracer::nvtx_profile_stats().entered_ranges, 0);
        gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
        stop.send(()).unwrap();
        assert!(server_thread.join().unwrap().is_empty());
        return;
    }
    let mut previous = 0;
    for _ in 0..3 {
        let elements: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|name| {
                gst::ElementFactory::make("nvinfer")
                    .name(name)
                    .build()
                    .unwrap()
                    .downcast::<Inference>()
                    .unwrap()
            })
            .collect();
        for e in &elements {
            e.set_state(gst::State::Paused).unwrap();
        }
        let domains: Vec<_> = elements
            .iter()
            .map(|e| e.imp().domain.load(Relaxed))
            .collect();
        assert!(domains[0] > previous && domains[1] > domains[0]);
        previous = domains[1];
        let workers: Vec<_> = [false, true]
            .into_iter()
            .map(|reverse| {
                let (a, b) = if reverse {
                    (domains[1], domains[0])
                } else {
                    (domains[0], domains[1])
                };
                std::thread::spawn(move || unsafe {
                    let work = || {
                        if gpu {
                            (adapter.launch)();
                        }
                        if cpu {
                            burn_cpu();
                        }
                    };
                    assert_eq!((adapter.push)(a), 0);
                    work();
                    assert_eq!((adapter.push)(b), 0);
                    work();
                    assert_eq!((adapter.pop)(a), 0); // independent, overlapping domain stacks
                    work(); // still B, never the popped A
                    assert_eq!((adapter.pop)(b), 0);
                    work(); // worker reused outside any element
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        for e in &elements {
            e.set_state(gst::State::Null).unwrap();
        }
        drop(elements);
        assert_eq!(gstoteltracer::nvtx_profile_stats().active_domains, 0);
    }
    let start = Instant::now();
    assert!(gstoteltracer::shutdown_report(Duration::from_secs(5))
        .result()
        .is_ok());
    assert!(start.elapsed() < Duration::from_secs(5));
    let stats = gstoteltracer::profile_stats();
    assert_eq!(stats.samples > 0, cpu);
    assert_eq!(stats.correlated_samples, 0);
    assert_eq!(stats.active_threads, 0);
    assert_eq!(stats.registration_failures, 0);
    assert_eq!(stats.context_descriptions, 0);
    let nvtx = gstoteltracer::nvtx_profile_stats();
    assert_eq!(nvtx.bound_domains, 6);
    assert_eq!(nvtx.entered_ranges, 12);
    assert_eq!(nvtx.capacity_rejections, 0);
    #[cfg(feature = "gpu-profiles")]
    {
        let stats = gstoteltracer::gpu_profile_stats();
        assert_eq!(stats.kernels, if gpu { 24 } else { 0 });
        assert_eq!(stats.attributed_kernels, if gpu { 18 } else { 0 });
        assert_eq!(stats.correlated_kernels, 0);
        assert_eq!(
            stats.pending_contexts + stats.dropped_events + stats.dropped_records,
            0
        );
    }
    stop.send(()).unwrap();
    let mut gpu_totals = HashMap::<String, i64>::new();
    let mut cpu_totals = HashMap::<String, i64>::new();
    for (is_gpu, profile) in server_thread.join().unwrap() {
        for sample in profile.sample {
            assert!(!sample.label.iter().any(|l| matches!(
                profile.string_table[l.key as usize].as_str(),
                "span_id" | "trace_id"
            )));
            let names: Vec<_> = sample
                .location_id
                .iter()
                .map(|id| {
                    let location = profile.location.iter().find(|l| l.id == *id).unwrap();
                    let function = profile
                        .function
                        .iter()
                        .find(|f| f.id == location.line[0].function_id)
                        .unwrap();
                    profile.string_table[function.name as usize].as_str()
                })
                .collect();
            let owner = if names.iter().any(|n| n.contains("first [nvinfer]")) {
                "first"
            } else if names.iter().any(|n| n.contains("second [nvinfer]")) {
                "second"
            } else {
                "unattributed"
            };
            if is_gpu {
                assert_eq!(names.len(), 2);
            }
            *if is_gpu {
                &mut gpu_totals
            } else {
                &mut cpu_totals
            }
            .entry(owner.into())
            .or_default() += sample.value[0];
        }
    }
    if gpu {
        assert_eq!(gpu_totals["first"], 90_000_000);
        assert_eq!(gpu_totals["second"], 90_000_000);
        assert_eq!(gpu_totals["unattributed"], 60_000_000);
    } else {
        assert!(gpu_totals.is_empty());
    }
    if cpu {
        for owner in ["first", "second", "unattributed"] {
            assert!(cpu_totals[owner] > 0);
        }
    } else {
        assert!(cpu_totals.is_empty());
    }
}
