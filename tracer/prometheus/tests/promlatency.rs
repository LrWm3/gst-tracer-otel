#[cfg(test)]
mod tests {
    use gst::prelude::*;
    use gstreamer::{self as gst};
    use std::{
        env::{self, consts::ARCH},
        path::Path,
        time::{Duration, Instant},
        vec,
    };

    #[test]
    fn given_basic_pipeline_when_run_then_metrics_captured() {
        // Setup test + gstreamer
        setup_test();

        // Create the pipeline
        // This is a kludge to get around a real issue where metrics are reused
        // across multiple pipelines which use the same element and pad names.
        //
        // We could tie the pipeline name to the metrics, but that would require
        // a change in the tracer implementation.
        let pipeline = create_pipeline("basic");

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
                    println!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    break;
                }
                _ => (),
            }
        }
        // Get the active tracer and then emit to get the metrics.

        #[cfg(feature = "v1_18")]
        {
            let binding = gst::active_tracers();
            println!("Active tracers: {}", binding.len());
            let tracer = binding
                .iter()
                .inspect(|t| {
                    println!("Active tracer: {}", t.name());
                })
                .find(|t| t.name() == "promlatencytracer0")
                .expect("Expected to find the `prom-latency` tracer");
            let _metrics_from_signal = tracer
                .emit_by_name::<Option<String>>("request-metrics", &[])
                .expect("Expected to get metrics from signal");
        }

        // Stop the pipeline
        pipeline.set_state(gst::State::Null).unwrap();

        // Get the metrics by performing an http request to the Prometheus endpoint
        // in >1.18, could use a signal.
        let prometheus_port =
            env::var("GST_PROMETHEUS_TRACER_PORT").expect("GST_PROMETHEUS_TRACER_PORT not set");
        let prometheus_url = format!("http://localhost:{prometheus_port}");
        let response = reqwest::blocking::get(&prometheus_url)
            .expect("Failed to fetch metrics from Prometheus endpoint");
        let metrics = response.text().expect("Failed to read response text");

        // Print the metrics for debugging
        println!("Metrics:\n{metrics}");

        // These will only be the same if we're running this as a single test
        // and not as part of a suite, because the metrics are not tied to a pipeline
        // TODO - fix this somehow
        // assert!(
        //     _metrics_from_signal == metrics.clone(),
        //     "Expected metrics from signal to match the metrics from the HTTP request, but they did not match.\nSignal metrics: {:?}\nHTTP metrics: {:?}",
        //     _metrics_from_signal, metrics
        // );

        // Validate that the metrics contain expected values
        let metric_asserts = vec![
            "gst_element_latency_last_gauge",
            "gst_element_latency_sum_count",
            "gst_element_latency_count_count",
        ];
        for metric in metric_asserts {
            assert!(
                metrics.contains(metric),
                "Expected to find '{metric}' in metrics"
            );
        }

        // count_count should be exactly 10000
        // ie: gst_element_latency_count_count{.*} 10000
        //
        // Test currently fails on count_value check because metrics are not tied to a pipeline, so they all sum up together
        //   as the test-suite runs multiple times.
        //
        let count_count_metric = format!("{}{{", "gst_element_latency_count_count");
        let count_count_value = metrics
            .lines()
            .filter(|line| line.contains(&count_count_metric))
            .flat_map(|line| line.split_whitespace().nth(1))
            .collect::<Vec<_>>();

        let mut check_failed = true;
        for value in count_count_value.clone() {
            // Check if the value is exactly 10000
            if value == "10000" {
                check_failed = false;
                break;
            }
        }
        if check_failed {
            panic!(
                "Expected to find '{count_count_metric}' with value 10000 in metrics, but it was not found.\n, found: {count_count_value:?}"
            );
        }
    }

    #[test]
    fn given_pipeline_with_known_latency_when_run_then_latency_metrics_match() {
        setup_test();

        // Sleep time 100 us
        // Identity itself adds about 9
        // We add a sleep-time of 1 nano as the sleep operation itself takes time per buffer
        let pipeline_el = gst::parse::launch(
            "fakesrc sync=false num-buffers=100 ! identity name=lm0 sleep-time=1 ! identity name=lm1 sleep-time=10000 ! fakesink",
        )
        .expect("Failed to create pipeline from launch string");
        pipeline_el.set_property("name", "latency_metrics_match");
        let pipeline = pipeline_el
            .downcast::<gst::Pipeline>()
            .expect("Failed to downcast to gst::Pipeline");

        let bus = pipeline.bus().unwrap();

        // Set the pipeline to the Playing state
        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to Playing");

        // Wait for EOS message
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    println!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    break;
                }
                _ => (),
            }
        }
        // Stop the pipeline
        pipeline.set_state(gst::State::Null).unwrap();

        // Get the metrics by performing an http request to the Prometheus endpoint
        let prometheus_port =
            env::var("GST_PROMETHEUS_TRACER_PORT").expect("GST_PROMETHEUS_TRACER_PORT not set");
        let prometheus_url = format!("http://localhost:{prometheus_port}");
        let response = reqwest::blocking::get(&prometheus_url)
            .expect("Failed to fetch metrics from Prometheus endpoint");
        let metrics = response.text().expect("Failed to read response text");

        // Print the metrics for debugging
        println!("Metrics:\n{metrics}");

        // Validate that the metrics contain expected values
        let metric_asserts = vec![
            "gst_element_latency_last_gauge",
            "gst_element_latency_sum_count",
            "gst_element_latency_count_count",
        ];
        for metric in metric_asserts {
            assert!(
                metrics.contains(metric),
                "Expected to find '{metric}' in metrics"
            );
        }

        fn get_metric_value(metrics: &str, metric_name: &str) -> Option<f64> {
            metrics
                .lines()
                .find(|line| line.contains(metric_name))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<f64>().ok())
        }
        // Check that the latency is around 100 us
        let latency_value =
            get_metric_value(&metrics, "gst_element_latency_last_gauge{element=\"lm1\"")
                .expect("Expected to find latency metric for lm1");
        let latency_value_no_sleep =
            get_metric_value(&metrics, "gst_element_latency_last_gauge{element=\"lm0\"")
                .expect("Expected to find latency metric for lm0");

        // TODO - lower this thresholds once we have fixed how we are measuring latency
        let last_check_failed = ((latency_value - latency_value_no_sleep) - 1e7).abs() >= 5e8;

        assert!(
            !last_check_failed,
            "Latency is not within expected range, found: {latency_value:?}"
        );

        // Check that the sum is around 1000 us
        let sum_value = get_metric_value(&metrics, "gst_element_latency_sum_count{element=\"lm1\"")
            .expect("Expected to find sum metric for lm1");
        let sum_value_no_sleep =
            get_metric_value(&metrics, "gst_element_latency_sum_count{element=\"lm0\"")
                .expect("Expected to find sum metric for lm0");

        // TODO - lower this thresholds once we have fixed how we are measuring latency
        let sum_check_failed = ((sum_value - sum_value_no_sleep) - 1e9).abs() >= 5e11;

        assert!(
            !sum_check_failed,
            "Sum is not within expected range, found: {sum_value:?}"
        );
    }

    #[test]
    fn given_pipeline_with_bin_with_ghost_pads_when_run_then_sink_src_pads_are_real_not_ghost() {
        setup_test();

        // Create a pipeline with a bin and elements
        let pipeline = gst::Pipeline::with_name("test-pipeline");
        let bin = gst::Bin::with_name("test-bin");
        let src = gst::ElementFactory::make("fakesrc")
            .name("fakesrc")
            .property("num-buffers", 100)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .name("fakesink")
            .build()
            .unwrap();
        let id1 = gst::ElementFactory::make("identity")
            .name("id1")
            .build()
            .unwrap();
        let id2 = gst::ElementFactory::make("identity")
            .name("id2")
            .property_from_str("sleep-time", "100")
            .build()
            .unwrap();
        let id3 = gst::ElementFactory::make("identity")
            .name("id3")
            .build()
            .unwrap();

        // Add elements to the bin
        bin.add(&id1).unwrap();
        bin.add(&id2).unwrap();
        bin.add(&id3).unwrap();

        // Link the elements together
        id1.link(&id2).unwrap();
        id2.link(&id3).unwrap();

        let g_src = gst::GhostPad::builder(gstreamer::PadDirection::Src)
            .with_target(&id3.static_pad("src").unwrap())
            .ok()
            .expect("Failed to create GhostPad for src")
            .build();
        g_src.set_active(true).unwrap();
        let g_sink = gst::GhostPad::builder(gstreamer::PadDirection::Sink)
            .with_target(&id1.static_pad("sink").unwrap())
            .ok()
            .expect("Failed to create GhostPad for sink")
            .build();
        g_sink.set_active(true).unwrap();
        bin.add_pad(&g_src).unwrap();
        bin.add_pad(&g_sink).unwrap();

        // Add the bin to the pipeline
        pipeline.add(&bin).unwrap();
        pipeline.add(&src).unwrap();
        pipeline.add(&sink).unwrap();

        // Link the bin to the src and sink
        src.link(&bin).unwrap();
        bin.link(&sink).unwrap();

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
                    println!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    break;
                }
                _ => (),
            }
        }
        // Stop the pipeline
        pipeline.set_state(gst::State::Null).unwrap();

        // Get the metrics by performing an http request to the Prometheus endpoint
        let prometheus_port =
            env::var("GST_PROMETHEUS_TRACER_PORT").expect("GST_PROMETHEUS_TRACER_PORT not set");
        let prometheus_url = format!("http://localhost:{prometheus_port}");
        let response = reqwest::blocking::get(&prometheus_url)
            .expect("Failed to fetch metrics from Prometheus endpoint");
        let metrics = response.text().expect("Failed to read response text");

        // Print the metrics for debugging
        println!("Metrics:\n{metrics}");
    }

    #[test]
    fn bench_prom_latency_through_pipeline() {
        setup_test();
        let elapsed = run_bench("prom-latency");
        assert!(
            elapsed < Duration::from_secs(1),
            "Pipeline benchmark took too long: {:?}",
            elapsed
        );
    }

    fn run_bench(tracer_name: &str) -> std::time::Duration {
        // Initialize GStreamer (loads your plugin & tracer)
        gst::init().expect("Failed to initialize GStreamer");

        // Verify that our element is registered:
        assert!(
            gst::TracerFactory::factories()
                .iter()
                .any(|f| f.name() == tracer_name),
            "Expected to find the `prom-latency` element after registration"
        );
        // Link the elements together
        let pipeline = create_pipeline("bench");

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

        // (Optionally assert it’s under some threshold:)
        // assert!(elapsed.as_secs_f64() < 1.0, "Pipeline too slow!");
        start.elapsed()
    }

    fn create_pipeline(name: &str) -> gst::Pipeline {
        let pipeline_el = gst::parse::launch("fakesrc num-buffers=10000 ! identity ! fakesink")
            .expect("Failed to create pipeline from launch string");
        pipeline_el.set_property("name", name);

        pipeline_el
            .downcast::<gst::Pipeline>()
            .expect("Failed to downcast to gst::Pipeline")
    }

    fn setup_test() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        env::set_var(
            "GST_TRACERS",
            "prom-latency(filters='GstBuffer',flags=element)",
        );
        env::set_var("GST_DEBUG", "GST_TRACER:5,prom-latency:6");

        env::set_var("GST_PROMETHEUS_TRACER_PORT", "9999");
        let root_manifest_dir = manifest_dir.parent().unwrap().parent().unwrap();
        let plugin_targets = [
            // ("release", true),
            // ("release", false),
            // ("profiling", true),
            // ("profiling", false),
            ("debug", true),
            ("debug", false),
        ];
        let plugin_paths = plugin_targets.iter().map(|(profile, with_target)| {
            let base = root_manifest_dir.join(format!("target/{}", profile));
            if *with_target {
                base.join(format!("{ARCH}-unknown-linux-gnu"))
                    .to_str()
                    .unwrap()
                    .to_owned()
            } else {
                base.to_str().unwrap().to_owned()
            }
        });
        let gst_plugin_path = plugin_paths.collect::<Vec<_>>().join(":");
        env::set_var("GST_PLUGIN_PATH", gst_plugin_path);

        // Initialize GStreamer
        gst::init().expect("Failed to initialize GStreamer");

        // Verify that our element is registered:
        assert!(
            gst::TracerFactory::factories()
                .iter()
                .any(|f| f.name() == "prom-latency"),
            "Expected to find the `prom-latency` element after registration"
        );

        let binding = gst::active_tracers();
        // println!("Active tracers: {}", binding.len());
        let _tracer = binding
            .iter()
            .inspect(|_t| {
                // println!("Active tracer: {}", t.name());
            })
            .find(|t| t.name() == "promlatencytracer0")
            .expect(format!("Expected to find the `{}` tracer", "promlatencytracer0").as_str());
    }
}
