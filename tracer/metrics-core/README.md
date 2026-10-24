# Shared Mars metrics collector

This crate contains collection and series lifecycle code used by `mars-metrics`
and explicit metrics signals in `mars-instrument`. It registers no GStreamer
plugin or GObject type. The wrappers own their tracer factories and exporters.

`metricsimp.rs` keeps the existing push/list/pull timing, per-pad I/O, PTS age,
queue sampling, cache invalidation and weak-owner retirement rules. The standalone
wrapper retains its text endpoint and action signal. The combined wrapper takes
snapshots for OTLP or OpenMetrics encoding on its export worker.

Optional callbacks surround ordinary push measurement in a defined order: trace
capture starts before timing and finishes after timing. Only a locally admitted
span supplies exemplar IDs. No callbacks run while a metric TLS borrow is held.
`exemplars.rs` stores a recent observation per histogram using nonblocking updates;
weak registry entries do not retain GStreamer objects.

`background.rs` makes maintenance and queue sampling interruptible. Shutdown is
permanent for this collector instance. Callers stop streaming before the final
snapshot and stop background tasks within their remaining shutdown deadline.
Protocol encoding and exporter lifecycle live in the OTel plugin's
`shared_metrics_wire.rs` and `shared_metrics.rs`.
