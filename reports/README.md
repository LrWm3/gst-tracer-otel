# Performance reports

Reviews, measured results, and supporting data are grouped by implementation.
Run reproduction commands from the repository root.

Device descriptions use "an integrated memory nvidia device". Identifying host
and hardware inventory has been removed from archived metadata; measurements
and original source/binary checksums are retained.

## Metrics

- [Metrics-only CPU and memory comparison](prometheus/METRICS_RESULTS.md)
- [Benchmarks on an integrated memory nvidia device and lifecycle checks](prometheus/BENCHMARK.md)
- [Rust review and cleanup findings](prometheus/REVIEW.md)

## OpenTelemetry and combined instrumentation

- [Original OpenTelemetry performance review](otel/OTEL_PERFORMANCE_REVIEW.md)
- [Compact tracing implementation](otel/RESULTS.md)
- [Compact capture follow-up](otel/FOLLOWUP_RESULTS.md)
- [OpenTelemetry SDK upgrade](otel/UPGRADE_RESULTS.md)
- [Direct OTLP encoding](otel/DIRECT_RESULTS.md)
- [Batch size, gzip and bounded trace-export concurrency](otel/DIRECT_RESULTS.md#bounded-export-tuning--2026-10-25)
- [Current stock-GStreamer capture costs](otel/DIRECT_RESULTS.md#current-stock-gstreamer-capture-check--2026-10-25)
- [Queue boundary delay, drops, flushes, and seeks](otel/DIRECT_RESULTS.md#queue-residence-investigation--2026-10-25)
- [Capture propagation](otel/CAPTURE_RESULTS.md)
- [Rejected-push overhead](otel/REJECTED_RESULTS.md)
- [Combined instrumentation measurements](otel/INSTRUMENT_RESULTS.md)
- [Current full-metrics, compatibility, and monitoring-level video matrix](otel/INSTRUMENT_VIDEO_RESULTS.md#current-full-metrics-signal-matrix--2026-10-25)
- [Current CPU profiling and log overhead on 1080p video](otel/PROFILE_VIDEO_RESULTS.md)
- [Cold/warm symbolization and function-only memory costs](otel/PROFILE_VIDEO_RESULTS.md#symbolization-cost-and-cache-retention--2026-10-25)
- [Thirty-minute native lifecycle soaks: memory, outages and shutdown](otel/PROFILE_VIDEO_RESULTS.md#thirty-minute-lifecycle-soaks--2026-10-25)
- [Element-attributed profiles: Grafana screenshots and validation](otel/PROFILE_ELEMENT_REVIEW.md)
- [Grafana trace/profile correlation: provisioning and backend validation](otel/CORRELATION_REVIEW.md)
- [Metric → trace → log → trace → profile walkthrough and validation](otel/SIGNALS_REVIEW.md#four-signal-walkthrough--2026-10-25)
- [Linux arm64 profiling: validation on an integrated memory nvidia device, CPU costs, and lifecycle checks](otel/ARM64_PROFILES.md)
- [Optional GPU profiles: inference workload overhead, memory percentages, and NVTX attribution cost](otel/GPU_PROFILE_RESULTS.md)

Historical correlated-profile measurements are described in the
[tracer guide](../tracer/otel/README.md#performance-and-sampling-cost), with
[initial raw measurements](otel/PROFILE_RESULTS.csv) and
[worker-optimization measurements](otel/PROFILE_OPTIMIZATION_RESULTS.csv).
