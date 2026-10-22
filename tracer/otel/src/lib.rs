/* Derived from gstlatency.c: tracing module that logs processing latency stats
 *
 * This library is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Library General Public
 * License as published by the Free Software Foundation; either
 * version 2 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Library General Public License for more details.
 *
 * You should have received a copy of the GNU Library General Public
 * License along with this library; if not, write to the
 * Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
 * Boston, MA 02110-1301, USA.
 */
use gstreamer as gst;
mod capture;
mod export;
mod metadata;
mod metrics;
mod otellogbridge;
mod profiles;
#[cfg(feature = "profiles")]
mod profiletracer;
mod propagation;
mod tracer;
mod transport;
mod wire;

pub use export::{ExportStats, FlushError};
pub use metrics::MetricStats;
pub use profiles::{profile_stats, ProfileStats};
#[cfg(feature = "profiles")]
pub use profiletracer::ProfilesTracer;
pub use tracer::{InstrumentTracer, TelemetryTracer};

/// Drain traces and snapshot cumulative metrics, bounded by `timeout`.
/// Stop streaming first for an exact final snapshot. Logs have a separate SDK flush.
pub fn flush(timeout: std::time::Duration) -> Result<ExportStats, FlushError> {
    tracer::runtime()?.flush(timeout)
}

/// Stop accepting records, drain exporters and stop the optional CPU profiler.
/// The timeout covers all cleanup; timed-out workers may continue in background.
/// Profile HTTP delivery acknowledgements are not exposed by the Pyroscope SDK.
pub fn shutdown(timeout: std::time::Duration) -> Result<ExportStats, FlushError> {
    let start = std::time::Instant::now();
    let result = tracer::runtime()?.shutdown(timeout);
    otellogbridge::shutdown(timeout.saturating_sub(start.elapsed()));
    let profiles = profiles::shutdown(timeout.saturating_sub(start.elapsed()));
    result.and_then(|stats| profiles.map(|()| stats))
}

/// Context of the current push, for explicit application instrumentation bridges.
/// Capture does not attach a live span to the OpenTelemetry thread-local context.
pub fn current_span_context() -> Option<opentelemetry::trace::SpanContext> {
    capture::current_context().filter(|context| context.is_valid())
}

/// Cumulative counters; a concurrent snapshot can span multiple updates.
pub fn stats() -> Result<ExportStats, FlushError> {
    Ok(tracer::runtime()?.stats.snapshot())
}

/// Metrics accounting, or zero counters when metrics are disabled.
pub fn metric_stats() -> Result<MetricStats, FlushError> {
    Ok(tracer::runtime()?
        .metrics
        .as_ref()
        .map_or_else(MetricStats::default, |m| m.stats()))
}

// ───────────────── plugin boilerplate ──────────────────
pub fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    tracer::register(plugin)?;
    #[cfg(feature = "profiles")]
    profiletracer::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    oteltracer, // → libgstoteltracer.so
    "Mars OTLP instrumentation and CPU profiling",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "LGPL",
    "gst_opentelemetry_tracer",
    "gst_opentelemetry_tracer",
    "https://github.com/LrWm3/gst-tracer-otel"
);

#[cfg(test)]
mod registration_tests {
    use super::*;
    use gst::prelude::*;

    #[test]
    fn registers_mars_entry_points() {
        gst::init().unwrap();
        plugin_register_static().unwrap();
        let mut factories = gst::TracerFactory::factories()
            .into_iter()
            .filter(|factory| factory.plugin_name().as_deref() == Some("oteltracer"))
            .map(|factory| {
                (
                    factory.name().to_string(),
                    factory.tracer_type().name().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        factories.sort();
        let expected = vec![
            (
                "mars-instrument".to_owned(),
                "MarsInstrumentTracer".to_owned(),
            ),
            ("mars-otel".to_owned(), "MarsOtelTracer".to_owned()),
            #[cfg(feature = "profiles")]
            ("mars-profiles".to_owned(), "MarsProfilesTracer".to_owned()),
        ];
        assert_eq!(factories, expected);
    }
}
