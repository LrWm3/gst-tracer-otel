use gst::prelude::*;
use gstreamer as gst;
use std::{
    env,
    net::TcpListener,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

// Initialize once, before GStreamer starts threads. Load the cdylib produced by
// this cargo test invocation. The sanitizer helper supplies newer Cargo layouts.
fn setup() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let executable = env::current_exe().unwrap();
        let filename = format!(
            "{}gstprometheustracer{}",
            env::consts::DLL_PREFIX,
            env::consts::DLL_SUFFIX
        );
        let direct = executable.parent().unwrap().join(&filename);
        let plugin = env::var_os("GST_PROM_TEST_PLUGIN").map(std::path::PathBuf::from).unwrap_or(direct);
        assert!(plugin.is_file(), "test plugin not found; use test_sanitizer.py for Cargo layouts without adjacent cdylibs");
        env::set_var("GST_PLUGIN_PATH", plugin.parent().unwrap());
        env::set_var(
            "GST_REGISTRY",
            env::temp_dir().join(format!("gst-prom-tests-{}.bin", std::process::id())),
        );
        env::set_var("GST_TRACERS", format!("mars-metrics(port={port})"));
        let case = env::var("GST_PROM_TEST_CASE").unwrap_or_default();
        if case == "sampling" {
            env::remove_var("GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE");
        } else {
            env::set_var("GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE", "1");
        }
        env::set_var(
            "GST_PROM_LATENCY_INCLUDE",
            if case == "filter" { "selected*" } else { "*" },
        );
        env::set_var(
            "GST_PROM_LATENCY_PTS_AGE",
            if case == "pts" { "true" } else { "false" },
        );
        env::set_var("GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS", "10");
        env::set_var(
            "GST_PROM_LATENCY_IO_COUNTS",
            if matches!(case.as_str(), "io" | "pull" | "filter") {
                "true"
            } else {
                "false"
            },
        );
        drop(listener);
        gst::init().unwrap();
        let factory = gst::TracerFactory::factories()
            .into_iter()
            .find(|factory| factory.name() == "mars-metrics")
            .expect("test plugin was not loaded");
        assert_eq!(factory.tracer_type().name(), "MarsMetricsTracer");
        assert_eq!(
            gst::TracerFactory::factories()
                .into_iter()
                .filter(|f| f.plugin_name() == factory.plugin_name())
                .count(),
            1,
            "the metrics plugin should register only its Mars entry point"
        );
        assert_eq!(
            factory
                .plugin()
                .unwrap()
                .filename()
                .unwrap()
                .canonicalize()
                .unwrap(),
            plugin.canonicalize().unwrap()
        );
        port
    })
}

fn pipeline(name: &str, description: &str) -> gst::Pipeline {
    setup();
    let pipeline = gst::parse::launch(description)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    pipeline.set_property("name", name);
    pipeline
}

struct StopOnDrop<'a>(&'a gst::Pipeline);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn wait_for_eos(pipeline: &gst::Pipeline) {
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline did not finish within 10 seconds");
    if let gst::MessageView::Error(error) = message.view() {
        panic!("pipeline error: {} ({:?})", error.error(), error.debug());
    }
}

fn run(pipeline: &gst::Pipeline) {
    let _stop = StopOnDrop(pipeline);
    pipeline.set_state(gst::State::Playing).unwrap();
    wait_for_eos(pipeline);
}

fn metrics() -> String {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match client
            .get(format!("http://127.0.0.1:{}/metrics", setup()))
            .send()
        {
            Ok(response) => return response.error_for_status().unwrap().text().unwrap(),
            Err(error) if Instant::now() >= deadline => panic!("metrics server: {error}"),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn value(text: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
    let mut labels = labels.to_vec();
    labels.sort_unstable();
    let labels = labels
        .iter()
        .map(|(key, value)| format!("{key}=\"{value}\""))
        .collect::<Vec<_>>()
        .join(",");
    let prefix = format!("{metric}{{{labels}}} ");
    let matches = text
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one series: {prefix}\n{text}"
    );
    matches[0].parse().unwrap()
}

fn pad_value(text: &str, metric: &str, element: &gst::Element, src_pad: &str) -> f64 {
    value(
        text,
        metric,
        &[
            ("element", element.name().as_str()),
            ("path", element.parent().unwrap().path_string().as_str()),
            ("sink_pad", "sink"),
            ("src_pad", src_pad),
        ],
    )
}

fn count(text: &str, element: &gst::Element, expected: u32) {
    assert_eq!(
        pad_value(text, "gst_element_latency_count_count", element, "src"),
        f64::from(expected)
    );
}

#[test]
fn exact_counts_and_signal_match_http_for_a_completed_pipeline() {
    let pipeline = pipeline(
        "basic",
        "fakesrc num-buffers=10000 ! identity name=work ! fakesink name=sink",
    );
    run(&pipeline);
    let text = metrics();
    for name in ["work", "sink"] {
        let element = pipeline.by_name(name).unwrap();
        count(&text, &element, 10000);
        assert_eq!(
            pad_value(
                &text,
                "gst_element_sync_wall_seconds_count",
                &element,
                "src"
            ),
            10000.0
        );
    }
    #[cfg(feature = "v1_18")]
    {
        let signal = gst::active_tracers()
            .iter()
            .find(|tracer| tracer.type_().name() == "MarsMetricsTracer")
            .unwrap()
            .emit_by_name::<Option<String>>("metrics", &[])
            .unwrap();
        // Other tests may still be streaming; compare this pipeline's series.
        for name in ["work", "sink"] {
            count(&signal, &pipeline.by_name(name).unwrap(), 10000);
        }
    }
}

#[test]
fn sleeping_element_latency_is_positive_and_excludes_downstream_work() {
    let pipeline = pipeline(
        "known_latency",
        "fakesrc num-buffers=30 ! identity name=fast ! identity name=slow sleep-time=10000 ! fakesink",
    );
    run(&pipeline);
    let text = metrics();
    let fast = pipeline.by_name("fast").unwrap();
    let slow = pipeline.by_name("slow").unwrap();
    count(&text, &fast, 30);
    count(&text, &slow, 30);
    let slow_sum = pad_value(&text, "gst_element_latency_sum_count", &slow, "src");
    let fast_sum = pad_value(&text, "gst_element_latency_sum_count", &fast, "src");
    assert!(slow_sum >= 30.0 * 8_000_000.0, "slow sum: {slow_sum}");
    assert!(
        fast_sum < slow_sum / 2.0,
        "fast={fast_sum}, slow={slow_sum}"
    );
    assert!(pad_value(&text, "gst_element_latency_last_gauge", &slow, "src") >= 8_000_000.0);
}

#[test]
fn sequential_tee_branches_are_both_subtracted() {
    let pipeline = pipeline(
        "branches",
        "fakesrc num-buffers=20 ! tee name=tee \
         tee. ! identity name=first sleep-time=10000 ! fakesink async=false sync=false \
         tee. ! identity name=second sleep-time=20000 ! fakesink async=false sync=false",
    );
    run(&pipeline);
    let text = metrics();
    let tee = pipeline.by_name("tee").unwrap();
    count(&text, &tee, 20);
    let tee_sum = pad_value(&text, "gst_element_latency_sum_count", &tee, "src");
    let first_sum = pad_value(
        &text,
        "gst_element_latency_sum_count",
        &pipeline.by_name("first").unwrap(),
        "src_0",
    );
    assert!(first_sum >= 20.0 * 8_000_000.0);
    assert!(
        tee_sum < first_sum / 2.0,
        "tee={tee_sum}, first branch={first_sum}"
    );
}

#[test]
fn ghost_forwarding_exports_each_real_sink_once_with_real_pad_names() {
    let pipeline = pipeline(
        "ghosts",
        "fakesrc num-buffers=10 ! ( identity name=inner1 ! identity name=inner2 ) ! fakesink name=sink",
    );
    run(&pipeline);
    let text = metrics();
    for name in ["inner1", "inner2", "sink"] {
        let element = pipeline.by_name(name).unwrap();
        count(&text, &element, 10);
        let rows = text.lines().filter(|line| {
            line.starts_with(&format!(
                "gst_element_latency_count_count{{element=\"{name}\","
            )) && line.contains("/GstPipeline:ghosts")
        });
        assert_eq!(rows.count(), 1, "duplicate forwarding series for {name}");
    }
}

fn detached_bin() -> gst::Bin {
    let bin = gst::Bin::with_name("inner");
    let source = gst::ElementFactory::make("fakesrc")
        .name("source")
        .property("num-buffers", 3i32)
        .build()
        .unwrap();
    let identity = gst::ElementFactory::make("identity")
        .name("id")
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .name("sink")
        .build()
        .unwrap();
    bin.add_many([&source, &identity, &sink]).unwrap();
    gst::Element::link_many([&source, &identity, &sink]).unwrap();
    bin
}

#[test]
fn bins_linked_before_parenting_have_distinct_pipeline_paths() {
    setup();
    let first = gst::Pipeline::with_name("late_first");
    let second = gst::Pipeline::with_name("late_second");
    first.add(&detached_bin()).unwrap();
    second.add(&detached_bin()).unwrap();
    run(&first);
    run(&second);
    let text = metrics();
    for pipeline in [&first, &second] {
        for name in ["id", "sink"] {
            count(&text, &pipeline.by_name(name).unwrap(), 3);
        }
    }
    assert!(
        !text.contains("path=\"/GstBin:inner\""),
        "premature labels: {text}"
    );
}

#[test]
fn reparenting_refreshes_existing_pad_labels() {
    setup();
    let first = gst::Pipeline::with_name("parent_before");
    let second = gst::Pipeline::with_name("parent_after");
    let bin = detached_bin();
    first.add(&bin).unwrap();
    run(&first);
    let before = metrics();
    count(&before, &bin.by_name("id").unwrap(), 3);
    first.remove(&bin).unwrap();
    second.add(&bin).unwrap();
    run(&second);
    let after = metrics();
    count(&after, &bin.by_name("id").unwrap(), 3);
    assert!(
        !after.contains("/GstPipeline:parent_before/"),
        "old path was not retired"
    );
}

#[test]
fn traced_elements_are_released_after_pipeline_teardown() {
    let pipeline = pipeline(
        "teardown",
        "fakesrc num-buffers=5 ! identity name=work ! fakesink name=sink",
    );
    let weak_elements = ["work", "sink"].map(|name| pipeline.by_name(name).unwrap().downgrade());
    run(&pipeline);
    drop(pipeline);
    for weak in weak_elements {
        assert!(
            weak.upgrade().is_none(),
            "tracer retained a destroyed element"
        );
    }
}

#[test]
fn sleeping_element_reports_blocked_time() {
    let pipeline = pipeline(
        "blocked",
        "fakesrc num-buffers=20 ! identity name=sleep sleep-time=10000 ! fakesink",
    );
    run(&pipeline);
    let text = metrics();
    let sleep = pipeline.by_name("sleep").unwrap();
    let wall = pad_value(&text, "gst_element_sync_wall_seconds_sum", &sleep, "src");
    let cpu = pad_value(&text, "gst_element_sync_cpu_seconds_sum", &sleep, "src");
    let blocked = pad_value(&text, "gst_element_sync_blocked_seconds_sum", &sleep, "src");
    assert!(wall >= 0.16, "sampled wall time was {wall}");
    assert!(blocked > cpu * 4.0, "blocked={blocked}, cpu={cpu}");
    assert!(blocked <= wall);
}

#[test]
fn queue2_completes_without_unsupported_signal_registration() {
    run(&pipeline(
        "queue2",
        "fakesrc num-buffers=10 ! queue2 ! fakesink",
    ));
}

#[test]
fn queue_depth_is_exported_with_pipeline_path() {
    let pipeline = pipeline("queue_levels", "fakesrc num-buffers=500 ! queue name=observable_queue max-size-buffers=500 max-size-bytes=0 max-size-time=0 ! identity sleep-time=10000 ! fakesink");
    let _stop = StopOnDrop(&pipeline);
    pipeline.set_state(gst::State::Playing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = metrics();
        if text.contains("gst_queue_current_level_buffers{element=\"observable_queue\"") {
            let depth = value(
                &text,
                "gst_queue_current_level_buffers",
                &[
                    ("element", "observable_queue"),
                    (
                        "path",
                        "/GstPipeline:queue_levels/GstQueue:observable_queue",
                    ),
                ],
            );
            if depth > 0.0 {
                break;
            }
        }
        assert!(Instant::now() < deadline, "queue depth was never exported");
        thread::sleep(Duration::from_millis(10));
    }
}

// Keep the entry point used by the just/perf recipes, without a machine-speed
// assertion in the correctness suite. Matched release runs use bench_matched.py.
#[test]
fn bench_prom_latency_through_pipeline() {
    let pipeline = pipeline("bench", "fakesrc num-buffers=10000 ! identity ! fakesink");
    let start = Instant::now();
    run(&pipeline);
    println!("10,000 buffers: {:?}", start.elapsed());
}

#[test]
fn destroyed_graph_series_are_retired_without_affecting_live_graphs() {
    let live = pipeline(
        "retirement_live",
        "fakesrc num-buffers=3 ! identity name=work ! fakesink",
    );
    run(&live);
    for n in 0..32 {
        let graph = pipeline(
            &format!("retirement_dead_{n}"),
            "fakesrc num-buffers=3 ! identity ! fakesink",
        );
        run(&graph);
        drop(graph);
    }
    let text = metrics();
    assert!(
        !text.contains("retirement_dead_"),
        "destroyed graphs still exported"
    );
    count(&text, &live.by_name("work").unwrap(), 3);
}

#[test]
fn queue_labels_follow_reparenting_and_disappear_after_teardown() {
    setup();
    let first = gst::Pipeline::with_name("queue_before");
    let second = gst::Pipeline::with_name("queue_after");
    let bin = gst::parse::bin_from_description("fakesrc is-live=true ! queue name=moved_queue ! identity sleep-time=1000 ! fakesink sync=false async=false", false).unwrap();
    bin.set_property("name", "inner");
    first.add(&bin).unwrap();
    for pipeline in [&first, &second] {
        let _stop = StopOnDrop(pipeline);
        pipeline.set_state(gst::State::Playing).unwrap();
        let expected = format!(
            "/GstPipeline:{}/GstBin:inner/GstQueue:moved_queue",
            pipeline.name()
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let text = metrics();
            if text.contains(&format!(
                "gst_queue_current_level_buffers{{element=\"moved_queue\",path=\"{expected}\"}}"
            )) {
                if pipeline == &second {
                    assert!(!text.lines().any(|line| line.starts_with("gst_queue_")
                        && line.contains("/GstPipeline:queue_before/")));
                }
                break;
            }
            assert!(Instant::now() < deadline, "queue labels did not refresh");
            thread::sleep(Duration::from_millis(10));
        }
        // Retire a path and return to it between sampler ticks. The cached
        // queue handles must be reacquired even if the final labels match.
        let original_name = pipeline.name();
        pipeline.set_property("name", "queue_detour");
        assert!(!metrics().contains(&format!("path=\"{expected}\"")));
        pipeline.set_property("name", original_name.as_str());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !metrics().contains(&format!("path=\"{expected}\"")) {
            assert!(
                Instant::now() < deadline,
                "queue handles were not reacquired"
            );
            thread::sleep(Duration::from_millis(10));
        }
        pipeline.set_state(gst::State::Null).unwrap();
        if pipeline == &first {
            first.remove(&bin).unwrap();
            second.add(&bin).unwrap();
        }
    }
    drop(bin);
    drop(first);
    drop(second);
    assert!(!metrics().contains("moved_queue"));
}

#[test]
fn configuration_variants() {
    for case in ["sampling", "filter", "pts", "io", "pull", "pull_without_io"] {
        let mut child = std::process::Command::new(env::current_exe().unwrap())
            .args(["--exact", "configuration_child", "--nocapture"])
            .env("GST_PROM_TEST_CASE", case)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "configuration {case} failed");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("configuration {case} timed out");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn configuration_child() {
    let Ok(case) = env::var("GST_PROM_TEST_CASE") else {
        return;
    };
    match case.as_str() {
        "sampling" => {
            let graph = pipeline(
                "sample_default",
                "fakesrc num-buffers=16384 ! identity name=work ! fakesink",
            );
            run(&graph);
            let text = metrics();
            let work = graph.by_name("work").unwrap();
            count(&text, &work, 16384);
            let sampled = pad_value(&text, "gst_element_sync_wall_seconds_count", &work, "src");
            assert!((700.0..1400.0).contains(&sampled), "sampled={sampled}");
            assert!(!text.contains("gst_buffer_pts_age_seconds"));
            assert!(!text.contains("gst_element_buffers_"));

            // More pads than cache slots forces eviction. Sampling must keep
            // advancing even when every buffer misses the thread-local cache.
            let stages: String = (0..96)
                .map(|index| format!(" ! identity name=stage{index}"))
                .collect();
            let deep = pipeline(
                "sample_eviction",
                &format!("fakesrc num-buffers=4096{stages} ! fakesink"),
            );
            run(&deep);
            let text = metrics();
            for index in 0..96 {
                let stage = deep.by_name(&format!("stage{index}")).unwrap();
                count(&text, &stage, 4096);
                let sampled =
                    pad_value(&text, "gst_element_sync_wall_seconds_count", &stage, "src");
                assert!((128.0..384.0).contains(&sampled), "stage{index}: {sampled}");
                for metric in [
                    "gst_element_sync_cpu_seconds_count",
                    "gst_element_sync_blocked_seconds_count",
                ] {
                    assert_eq!(pad_value(&text, metric, &stage, "src"), sampled);
                }
            }
        }
        "filter" => {
            let graph = pipeline("filter", "fakesrc num-buffers=20 ! identity name=selected_parent ! identity name=unselected sleep-time=10000 ! identity name=unselected_middle ! identity name=unselected_last ! identity name=selected_child ! fakesink");
            run(&graph);
            let text = metrics();
            let parent = graph.by_name("selected_parent").unwrap();
            count(&text, &parent, 20);
            assert_eq!(io_value(&text, "in", &parent, "sink"), 20.0);
            assert_eq!(io_value(&text, "out", &parent, "src"), 20.0);
            count(&text, &graph.by_name("selected_child").unwrap(), 20);
            assert!(!text.contains("element=\"unselected\""));
            assert!(!text.contains("element=\"fakesink0\""));
            assert!(
                pad_value(&text, "gst_element_latency_sum_count", &parent, "src") < 100_000_000.0
            );
            for name in ["selected_parent", "selected_child"] {
                let element = graph.by_name(name).unwrap();
                for metric in [
                    "gst_element_sync_wall_seconds_count",
                    "gst_element_sync_cpu_seconds_count",
                    "gst_element_sync_blocked_seconds_count",
                ] {
                    assert_eq!(pad_value(&text, metric, &element, "src"), 20.0);
                }
            }
        }
        "pts" => {
            let graph = pipeline("pts", "fakesrc num-buffers=20 is-live=true do-timestamp=true format=time ! identity sleep-time=1000 ! identity name=work ! fakesink sync=false");
            run(&graph);
            let text = metrics();
            let work = graph.by_name("work").unwrap();
            assert_eq!(
                pad_value(&text, "gst_buffer_pts_age_seconds_count", &work, "src"),
                20.0
            );
            assert!(pad_value(&text, "gst_buffer_pts_age_seconds_sum", &work, "src") >= 0.015);
        }
        "io" => io_counts_and_multiple_pads(),
        "pull" | "pull_without_io" => pull_latency_and_counts(case == "pull"),
        _ => panic!("unknown configuration: {case}"),
    }
}

fn io_value(text: &str, direction: &str, element: &gst::Element, pad: &str) -> f64 {
    value(
        text,
        &format!("gst_element_buffers_{direction}_total"),
        &[
            ("element", element.name().as_str()),
            ("path", element.parent().unwrap().path_string().as_str()),
            ("pad", pad),
        ],
    )
}

fn io_counts_and_multiple_pads() {
    let graph = pipeline("multi_io", "funnel name=merge ! tee name=split split.src_100 ! queue ! fakesink async=false split.src_101 ! queue ! fakesink async=false fakesrc num-buffers=7 ! merge.sink_100 fakesrc num-buffers=11 ! merge.sink_101");
    run(&graph);
    let text = metrics();
    let merge = graph.by_name("merge").unwrap();
    let split = graph.by_name("split").unwrap();
    assert_eq!(io_value(&text, "in", &merge, "sink_100"), 7.0);
    assert_eq!(io_value(&text, "in", &merge, "sink_101"), 11.0);
    assert_eq!(io_value(&text, "out", &merge, "src"), 18.0);
    assert_eq!(io_value(&text, "in", &split, "sink"), 18.0);
    for pad in ["src_100", "src_101"] {
        assert_eq!(io_value(&text, "out", &split, pad), 18.0);
    }
    for (pad, expected) in [("sink_100", 7.0), ("sink_101", 11.0)] {
        assert_eq!(
            value(
                &text,
                "gst_element_latency_count_count",
                &[
                    ("element", "merge"),
                    ("path", graph.path_string().as_str()),
                    ("src_pad", "src"),
                    ("sink_pad", pad)
                ]
            ),
            expected
        );
    }

    let retired_pad = split.static_pad("src_100").unwrap();
    split.release_request_pad(&retired_pad);
    let text = metrics();
    assert!(!text
        .lines()
        .any(|line| line
            .starts_with("gst_element_buffers_out_total{element=\"split\",pad=\"src_100\"")));
    assert_eq!(io_value(&text, "out", &split, "src_101"), 18.0);

    let ghost = pipeline(
        "ghost_io",
        "fakesrc num-buffers=7 ! ( identity name=inside ) ! fakesink name=end",
    );
    run(&ghost);
    let text = metrics();
    let inside = ghost.by_name("inside").unwrap();
    assert_eq!(io_value(&text, "in", &inside, "sink"), 7.0);
    assert_eq!(io_value(&text, "out", &inside, "src"), 7.0);
    assert_eq!(
        io_value(&text, "in", &ghost.by_name("end").unwrap(), "sink"),
        7.0
    );

    let dropped = pipeline(
        "drop_io",
        "fakesrc num-buffers=7 ! identity name=dropper drop-probability=1.0 ! fakesink async=false",
    );
    run(&dropped);
    let text = metrics();
    let dropper = dropped.by_name("dropper").unwrap();
    assert_eq!(io_value(&text, "in", &dropper, "sink"), 7.0);
    assert_eq!(io_value(&text, "out", &dropper, "src"), 0.0);

    // Direct pads make list size, failed pushes, and relinking deterministic.
    let graph = gst::Pipeline::with_name("lists_io");
    let source = gst::Bin::with_name("source");
    let first = gst::Bin::with_name("first");
    let second = gst::Bin::with_name("second");
    graph.add_many([&source, &first, &second]).unwrap();
    let src = gst::Pad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    let a = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink_100")
        .chain_function(|_, _, _| Ok(gst::FlowSuccess::Ok))
        .chain_list_function(|_, _, _| Ok(gst::FlowSuccess::Ok))
        .build();
    let b = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink_101")
        .chain_function(|_, _, _| Err(gst::FlowError::Error))
        .build();
    source.add_pad(&src).unwrap();
    first.add_pad(&a).unwrap();
    second.add_pad(&b).unwrap();
    src.link(&a).unwrap();
    for pad in [&src, &a, &b] {
        pad.set_active(true).unwrap();
    }
    push_events(&src);
    let mut list = gst::BufferList::new();
    for _ in 0..4 {
        list.get_mut().unwrap().add(gst::Buffer::new());
    }
    src.push_list(list).unwrap();
    src.unlink(&a).unwrap();
    src.link(&b).unwrap();
    push_events(&src);
    assert_eq!(src.push(gst::Buffer::new()), Err(gst::FlowError::Error));
    let text = metrics();
    assert_eq!(io_value(&text, "out", source.upcast_ref(), "src"), 5.0);
    assert_eq!(io_value(&text, "in", first.upcast_ref(), "sink_100"), 4.0);
    assert_eq!(io_value(&text, "in", second.upcast_ref(), "sink_101"), 1.0);
    for pad in [&src, &a, &b] {
        pad.set_active(false).unwrap();
    }
}

fn push_events(pad: &gst::Pad) {
    assert!(pad.push_event(gst::event::StreamStart::new("test")));
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(pad.push_event(gst::event::Segment::new(&segment)));
}

fn pull_latency_and_counts(io: bool) {
    setup();
    let graph = gst::Pipeline::with_name("pull_nested");
    let source = gst::Bin::with_name("source");
    let middle = gst::Bin::with_name("middle");
    let sink = gst::Bin::with_name("sink");
    let inner = gst::Bin::with_name("inner");
    inner.add(&source).unwrap();
    graph.add_many([&inner, &middle, &sink]).unwrap();
    let src = gst::Pad::builder(gst::PadDirection::Src)
        .name("src")
        .getrange_function(|_, _, offset, _, size| {
            if offset == 100 {
                return Err(gst::FlowError::Eos);
            }
            if offset == 101 {
                return Err(gst::FlowError::Error);
            }
            thread::sleep(Duration::from_millis(5));
            Ok(gst::PadGetRangeSuccess::NewBuffer(
                gst::Buffer::with_size(size as usize).unwrap(),
            ))
        })
        .build();
    let middle_in = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink")
        .build();
    let input = middle_in.downgrade();
    let middle_out = gst::Pad::builder(gst::PadDirection::Src)
        .name("src")
        .getrange_function(move |_, _, offset, _, size| {
            let buffer = input.upgrade().unwrap().pull_range(offset, size)?;
            thread::sleep(Duration::from_millis(2));
            Ok(gst::PadGetRangeSuccess::NewBuffer(buffer))
        })
        .build();
    let dst = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink")
        .build();
    source.add_pad(&src).unwrap();
    middle.add_pad(&middle_in).unwrap();
    middle.add_pad(&middle_out).unwrap();
    sink.add_pad(&dst).unwrap();
    let ghost = gst::GhostPad::builder_with_target(&src)
        .unwrap()
        .name("out")
        .build();
    inner.add_pad(&ghost).unwrap();
    ghost.link(&middle_in).unwrap();
    middle_out.link(&dst).unwrap();
    for pad in [ghost.upcast_ref(), &src, &middle_in, &middle_out, &dst] {
        pad.activate_mode(gst::PadMode::Pull, true).unwrap();
    }
    for offset in 0..10 {
        assert_eq!(dst.pull_range(offset, 16).unwrap().size(), 16);
    }
    assert_eq!(dst.pull_range(100, 16), Err(gst::FlowError::Eos));
    assert_eq!(dst.pull_range(101, 16), Err(gst::FlowError::Error));
    let text = metrics();
    for element in [&source, &middle] {
        assert_eq!(
            pad_value(
                &text,
                "gst_element_pull_calls_total",
                element.upcast_ref(),
                "src"
            ),
            12.0
        );
        for metric in [
            "gst_element_pull_wall_seconds_count",
            "gst_element_pull_cpu_seconds_count",
            "gst_element_pull_blocked_seconds_count",
        ] {
            assert_eq!(pad_value(&text, metric, element.upcast_ref(), "src"), 12.0);
        }
        let sum = pad_value(
            &text,
            "gst_element_pull_wall_nanoseconds_total",
            element.upcast_ref(),
            "src",
        );
        let histogram_sum = pad_value(
            &text,
            "gst_element_pull_wall_seconds_sum",
            element.upcast_ref(),
            "src",
        );
        assert!((sum - histogram_sum * 1e9).abs() < 1.0);
        assert!(
            pad_value(
                &text,
                "gst_element_pull_last_nanoseconds",
                element.upcast_ref(),
                "src"
            ) >= 0.0
        );
        if io {
            assert_eq!(io_value(&text, "out", element.upcast_ref(), "src"), 10.0);
        }
    }
    if io {
        assert_eq!(io_value(&text, "in", middle.upcast_ref(), "sink"), 10.0);
        assert_eq!(io_value(&text, "in", sink.upcast_ref(), "sink"), 10.0);
    } else {
        assert!(!text.contains("gst_element_buffers_"));
    }
    let source_wall = pad_value(
        &text,
        "gst_element_pull_wall_seconds_sum",
        source.upcast_ref(),
        "src",
    );
    let middle_wall = pad_value(
        &text,
        "gst_element_pull_wall_seconds_sum",
        middle.upcast_ref(),
        "src",
    );
    assert!(source_wall >= 0.04, "source={source_wall}");
    assert!(
        middle_wall >= 0.015 && middle_wall < source_wall * 0.8,
        "middle={middle_wall}, source={source_wall}"
    );
    assert!(!text.contains("gst_element_latency_count_count"));
    for pad in [&dst, &middle_out, &middle_in, ghost.upcast_ref(), &src] {
        pad.activate_mode(gst::PadMode::Pull, false).unwrap();
    }
}

#[test]
fn renaming_a_live_graph_refreshes_labels() {
    let graph = pipeline("rename_before", "fakesrc is-live=true ! identity name=work sleep-time=1000 ! fakesink sync=false async=false");
    let _stop = StopOnDrop(&graph);
    graph.set_state(gst::State::Playing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !metrics().contains("/GstPipeline:rename_before") {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    graph.set_property("name", "rename_after");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = metrics();
        if text.contains("/GstPipeline:rename_after") {
            assert!(!text.contains("/GstPipeline:rename_before"));
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
}
