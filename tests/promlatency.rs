// tests/bench_prom_latency.rs

#[cfg(test)]
mod tests {
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
        // Stop the pipeline
        pipeline.set_state(gst::State::Null).unwrap();

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
                "Expected to find '{}' with value 10000 in metrics, but it was not found.\n, found: {:?}",
                count_count_metric,
                count_count_value
            );
        }
    }

    #[test]
    fn given_pipeline_with_known_latency_when_run_then_latency_metrics_match() {
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

        // Sleep time 100 us
        // Identity itself adds about 9
        let pipeline_el = gst::parse::launch(
            "fakesrc num-buffers=100 ! identity name=lm0 sleep-time=10000 ! identity name=lm1 sleep-time=1 ! fakesink",
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

        fn get_metric_value(metrics: &str, metric_name: &str) -> Option<f64> {
            metrics
                .lines()
                .find(|line| line.contains(metric_name))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<f64>().ok())
        }
        // Check that the latency is around 100 us
        let latency_value =
            get_metric_value(&metrics, "gst_element_latency_last_gauge{element=\"lm0\"")
                .expect("Expected to find latency metric for lm0");
        let latency_value_no_sleep =
            get_metric_value(&metrics, "gst_element_latency_last_gauge{element=\"lm1\"")
                .expect("Expected to find latency metric for lm1");

        let check_failed = ((latency_value - latency_value_no_sleep) - 1e7).abs() >= 1e5;

        assert!(
            !check_failed,
            "Latency is not within expected range, found: {:?}",
            latency_value
        );

        // Check that the sum is around 1000 us
        let sum_value = get_metric_value(&metrics, "gst_element_latency_sum_count{element=\"lm0\"")
            .expect("Expected to find sum metric for lm0");
        let sum_value_no_sleep =
            get_metric_value(&metrics, "gst_element_latency_sum_count{element=\"lm1\"")
                .expect("Expected to find sum metric for lm1");

        let check_failed = ((sum_value - sum_value_no_sleep) - 1e9).abs() >= 1e7;
        assert!(
            !check_failed,
            "Sum is not within expected range, found: {:?}",
            sum_value
        );
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
        env::set_var("GST_PROMETHEUS_TRACER_PORT", "9999");
        env::set_var("GST_PLUGIN_PATH", env!("CARGO_MANIFEST_DIR"));
        env::set_var(
            "GST_TRACERS",
            "prom-latency(filters='GstBuffer',flags=element)",
        );
        env::set_var("GST_DEBUG", "GST_TRACER:5,prom-latency:6");

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
        let elapsed = start.elapsed();

        // (Optionally assert it’s under some threshold:)
        // assert!(elapsed.as_secs_f64() < 1.0, "Pipeline too slow!");
        elapsed
    }

    fn create_pipeline(name: &str) -> gst::Pipeline {
        let pipeline_el = gst::parse::launch("fakesrc num-buffers=10000 ! identity ! fakesink")
            .expect("Failed to create pipeline from launch string");
        pipeline_el.set_property("name", name);
        let pipeline = pipeline_el
            .downcast::<gst::Pipeline>()
            .expect("Failed to downcast to gst::Pipeline");
        pipeline
    }
}
