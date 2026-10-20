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
mod otellogbridge;
mod oteltracer;
mod propagation;

pub use export::{ExportStats, FlushError};
pub use oteltracer::TelemetryTracer;

/// Drain captured records and finish export attempts, bounded by `timeout`.
pub fn flush(timeout: std::time::Duration) -> Result<ExportStats, FlushError> {
    oteltracer::runtime()?.flush(timeout)
}

/// Stop accepting records, drain the worker, and shut down the exporter.
pub fn shutdown(timeout: std::time::Duration) -> Result<ExportStats, FlushError> {
    let start = std::time::Instant::now();
    let result = oteltracer::runtime()?.shutdown(timeout);
    otellogbridge::shutdown(timeout.saturating_sub(start.elapsed()));
    result
}

/// Context of the current push, for explicit application instrumentation bridges.
/// Capture does not attach a live span to the OpenTelemetry thread-local context.
pub fn current_span_context() -> Option<opentelemetry::trace::SpanContext> {
    capture::current_context().filter(|context| context.is_valid())
}

/// Cumulative counters; a concurrent snapshot can span multiple updates.
pub fn stats() -> Result<ExportStats, FlushError> {
    Ok(oteltracer::runtime()?.stats.snapshot())
}

// ───────────────── plugin boilerplate ──────────────────
pub fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    oteltracer::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    oteltracer, // → libgstoteltracer.so
    "GStreamer Open Telemetry tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "LGPL",
    "gst_opentelemetry_tracer",
    "gst_opentelemetry_tracer",
    "https://github.com/LrWm3/gst-tracer-otel"
);
