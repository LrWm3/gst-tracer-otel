// tests/bench_prom_latency.rs

use gst::prelude::*;
use gstreamer as gst;
use std::{env, time::Instant, vec};

#[test]
fn given_basic_pipeline_when_run_then_metrics_captured() {
    // Set environment variables for the tracer
    env::set_var(
        "GST_TRACERS",
        "prom-latency(filters='GstBuffer',flags=element)",
    );
    env::set_var("GST_DEBUG", "GST_TRACER:5,prom-latency:6");
    env::set_var("GST_PROMETHEUS_TRACER_PORT", "9999");
    env::set_var("GST_PLUGIN_PATH", env!("CARGO_MANIFEST_DIR"));

    // Initialize GStreamer
    gst::init().expect("Failed to initialize GStreamer");

    // Verify that our element is registered:
    assert!(
        gst::TracerFactory::factories()
            .iter()
            .find(|f| f.name() == "prom-latency")
            .is_some(),
        "Expected to find the `prom-latency` element after registration"
    );

    // Create the pipeline
    let pipeline = create_pipeline();

    // Set the pipeline to the Playing state
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to Playing");

    // Grab the bus to listen for EOS
    let bus = pipeline.bus().unwrap();

    // Wait for EOS message
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                // Stop the pipeline on error
                pipeline.set_state(gst::State::Null).unwrap();
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    // Get the metrics by performing an http request to the Prometheus endpoint
    // in >1.18, could use a signal.
    let prometheus_port =
        env::var("GST_PROMETHEUS_TRACER_PORT").expect("GST_PROMETHEUS_TRACER_PORT not set");
    let prometheus_url = format!("http://localhost:{}", prometheus_port);
    let response = reqwest::blocking::get(&prometheus_url)
        .expect("Failed to fetch metrics from Prometheus endpoint");
    let metrics = response.text().expect("Failed to read response text");

    // Print the metrics for debugging
    println!("Metrics:\n{}", metrics);

    // Validate that the metrics contain expected values
    let metric_asserts = vec![
        "gst_element_latency_last_gauge",
        "gst_element_latency_sum_count",
        "gst_element_latency_count_count",
    ];
    for metric in metric_asserts {
        assert!(
            metrics.contains(metric),
            "Expected to find '{}' in metrics",
            metric
        );
    }

    // count_count should be exactly 100000
    // ie: gst_element_latency_count_count{.*} 100000
    let count_count_metric = format!("{}{{", "gst_element_latency_count_count");
    let count_count_value = metrics
        .lines()
        .find(|line| line.contains(&count_count_metric))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("Failed to find count_count value in metrics");
    assert_eq!(
        count_count_value, "100000",
        "Expected count_count to be 100000, found {}",
        count_count_value
    );

    // Stop the pipeline
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn bench_no_trace_plugin() {
    // run bench 5 times and capture durations in a list
    let durations: Vec<_> = (0..5).map(|_| run_bench("latency")).collect();

    // Print the durations
    for (i, duration) in durations.iter().enumerate() {
        println!("Run {}: Duration: {:?}", i + 1, duration);
        // Optionally assert it’s under some threshold:
        // assert!(duration.as_secs_f64() < 1.0, "Pipeline too slow!");
    }
}

#[test]
fn bench_prom_latency_through_pipeline() {
    env::set_var("GST_PLUGIN_PATH", env!("CARGO_MANIFEST_DIR"));
    env::set_var(
        "GST_TRACERS",
        "prom-latency(filters='GstBuffer',flags=element)",
    );
    env::set_var("GST_DEBUG", "GST_TRACER:5,prom-latency:6");
    env::set_var("GST_PROMETHEUS_TRACER_PORT", "9999");

    // run bench 5 times and capture durations in a list
    let durations: Vec<_> = (0..5).map(|_| run_bench("prom-latency")).collect();

    // Print the durations
    for (i, duration) in durations.iter().enumerate() {
        println!("Run {}: Duration: {:?}", i + 1, duration);
        // Optionally assert it’s under some threshold:
        // assert!(duration.as_secs_f64() < 1.0, "Pipeline too slow!");
    }
}

#[test]
fn bench_latency_through_pipeline() {
    env::set_var("GST_TRACERS", "latency(filters='GstBuffer',flags=element)");
    env::set_var("GST_DEBUG", "GST_TRACER:5,latency:3");

    let durations: Vec<_> = (0..5).map(|_| run_bench("latency")).collect();

    // Print the durations
    for (i, duration) in durations.iter().enumerate() {
        println!("Run {}: Duration: {:?}", i + 1, duration);
        // Optionally assert it’s under some threshold:
        // assert!(duration.as_secs_f64() < 1.0, "Pipeline too slow!");
    }
}

fn run_bench(tracer_name: &str) -> std::time::Duration {
    // Initialize GStreamer (loads your plugin & tracer)
    gst::init().expect("Failed to initialize GStreamer");

    // Verify that our element is registered:
    assert!(
        gst::TracerFactory::factories()
            .iter()
            .find(|f| f.name() == tracer_name)
            .is_some(),
        "Expected to find the `prom-latency` element after registration"
    );
    // Link the elements together
    let pipeline = create_pipeline();

    // Start playing and benchmark from PLAYING -> EOS
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to Playing");

    // Grab the bus to listen for EOS
    let bus = pipeline.bus().unwrap();

    // Start wall-clock timer
    let start = Instant::now();

    // Block until we see an EOS message
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }

    // Stop the pipeline
    pipeline.set_state(gst::State::Null).unwrap();

    // Report elapsed time
    let elapsed = start.elapsed();

    // (Optionally assert it’s under some threshold:)
    // assert!(elapsed.as_secs_f64() < 1.0, "Pipeline too slow!");
    elapsed
}

fn create_pipeline() -> gst::Pipeline {
    let pipeline = gst::Pipeline::new();
    let fakesrc = gst::ElementFactory::make("fakesrc")
        .name("fakesrc")
        .property("num-buffers", &100_000)
        .build()
        .expect("Failed to create fakesrc");
    let identity = gst::ElementFactory::make("identity")
        .name("id")
        .build()
        .expect("Failed to create identity");
    let fakesink = gst::ElementFactory::make("fakesink")
        .name("fakesink")
        .build()
        .expect("Failed to create fakesink");

    // Add elements to the pipeline
    pipeline
        .add_many(&[&fakesrc, &identity, &fakesink])
        .expect("Failed to add elements to pipeline");

    // Link the elements together
    gst::Element::link_many(&[&fakesrc, &identity, &fakesink]).expect("Failed to link elements");

    pipeline
}
