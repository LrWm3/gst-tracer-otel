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
// this cargo test invocation, never an older release build or installed plugin.
fn setup() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let executable = env::current_exe().unwrap();
        env::set_var("GST_PLUGIN_PATH", executable.parent().unwrap());
        env::set_var(
            "GST_REGISTRY",
            env::temp_dir().join(format!("gst-prom-tests-{}.bin", std::process::id())),
        );
        env::set_var("GST_TRACERS", format!("prom-latency(port={port})"));
        env::set_var("GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE", "1");
        env::set_var("GST_PROM_LATENCY_INCLUDE", "*");
        env::set_var("GST_PROM_LATENCY_PTS_AGE", "false");
        env::set_var("GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS", "10");
        drop(listener);
        gst::init().unwrap();
        assert!(gst::TracerFactory::factories()
            .iter()
            .any(|factory| factory.name() == "prom-latency"));
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
            .find(|tracer| tracer.type_().name() == "promlatencytracer")
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
    assert_eq!(
        value(
            &after,
            "gst_element_latency_count_count",
            &[
                ("element", "id"),
                ("path", "/GstPipeline:parent_before/GstBin:inner"),
                ("sink_pad", "sink"),
                ("src_pad", "src"),
            ]
        ),
        3.0
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
