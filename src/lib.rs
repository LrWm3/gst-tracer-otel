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
use glib;
use gstreamer as gst;
mod nooplatency;
mod promlatency;

// ───────────────── plugin boilerplate ──────────────────
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the tracer factory
    promlatency::register(plugin)?;
    nooplatency::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    telemetytracer, // → libgsttelemetytracer.so
    "GStreamer telemetry latency tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MPL-2.0",
    "gst_telemetry_latency_tracer",
    "gst_telemetry_latency_tracer",
    "https://example.com"
);
