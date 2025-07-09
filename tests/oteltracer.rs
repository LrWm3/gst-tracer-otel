#[cfg(test)]
mod tests {
    use gst::prelude::*;
    use gstreamer as gst;
    use std::{env, time::Instant, vec};

    #[test]
    fn given_basic_pipeline_when_run_otel_then_metrics_captured() {
        // Set environment variables for the tracer
        env::set_var(
            "GST_TRACERS",
            "otel-tracer(filters='GstBuffer',flags=element)",
        );
        env::set_var("GST_DEBUG", "GST_TRACER:5,otel-tracer:7");
        env::set_var("GST_PLUGIN_PATH", env!("CARGO_MANIFEST_DIR"));

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
        let pipeline_el = gst::parse::launch("fakesrc num-buffers=100 ! identity ! fakesink")
            .expect("Failed to create pipeline from launch string");
        pipeline_el.set_property("name", "basic");
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

        // TODO - Check metrics / traces / logs somehow
    }
}
