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
mod noop;

// ───────────────── plugin boilerplate ──────────────────
pub fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    noop::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    nooptracer, // → libgstnooptracer.so
    "Mars no-op tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "LGPL",
    "gst_noop_tracer",
    "gst_noop_tracer",
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
            .filter(|factory| factory.plugin_name().as_deref() == Some("nooptracer"))
            .map(|factory| {
                (
                    factory.name().to_string(),
                    factory.tracer_type().name().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        factories.sort();
        assert_eq!(
            factories,
            vec![("mars-noop".to_owned(), "MarsNoopTracer".to_owned())]
        );
    }
}
