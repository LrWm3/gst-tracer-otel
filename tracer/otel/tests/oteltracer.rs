#[cfg(test)]
mod tests {
    use gst::prelude::*;
    use gstreamer as gst;
    use std::env;

    #[test]
    fn given_basic_pipeline_when_run_otel_then_metrics_captured() {
        help_run_gstreamer_tests(
            "basic",
            "fakesrc num-buffers=3 ! identity ! identity ! identity ! identity ! fakesink",
        );
    }

    #[test]
    fn given_mthread_pipeline_when_run_otel_then_traces_captured() {
        help_run_gstreamer_tests(
            "multithreaded",
            "fakesrc num-buffers=5 ! queue max-size-buffers=3 ! fakesink",
        );
    }

    #[test]
    fn given_pipeline_with_bin_element_when_run_otel_then_traces_captured() {
        // TODO will need to create a custom bin element, probably can't use help_run_gstreamer_tests directly
    }

    fn help_run_gstreamer_tests(name: &str, pipeline: &str) {
        // Set environment variables for the tracer
        env::set_var(
            "GST_TRACERS",
            "otel-tracer(filters='GstBuffer',flags=element)",
        );
        env::set_var(
            "GST_DEBUG",
            "fakesink:5,identity:5,GST_TRACER:5,otel-tracer:7",
        );
        // TODO - is there a better way?
        env::set_var("GST_PLUGIN_PATH", "../../target/release:../../target/debug");

        // Initialize GStreamer
        gst::init().expect("Failed to initialize GStreamer");

        // Verify that our element is registered:
        assert!(
            gst::TracerFactory::factories()
                .iter()
                .find(|f| f.name() == "otel-tracer")
                .is_some(),
            "Expected to find the `otel-tracer` element after registration"
        );

        // Create the pipeline
        // This is a kludge to get around a real issue where metrics are reused
        // across multiple pipelines which use the same element and pad names.
        //
        // We could tie the pipeline name to the metrics, but that would require
        // a change in the tracer implementation.
        let pipeline_el =
            gst::parse::launch(pipeline).expect("Failed to create pipeline from launch string");
        pipeline_el.set_property("name", name);
        let pipeline = pipeline_el
            .downcast::<gst::Pipeline>()
            .expect("Failed to downcast to gst::Pipeline");

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
    }
}
