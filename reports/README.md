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
- [Capture propagation](otel/CAPTURE_RESULTS.md)
- [Rejected-push overhead](otel/REJECTED_RESULTS.md)
- [Combined instrumentation measurements](otel/INSTRUMENT_RESULTS.md)
- [Combined instrumentation on 1080p video, including profiles and logs](otel/INSTRUMENT_VIDEO_RESULTS.md)
- [Current CPU profiling and log overhead on 1080p video](otel/PROFILE_VIDEO_RESULTS.md)
- [Element-attributed profiles: Grafana screenshots and validation](otel/PROFILE_ELEMENT_REVIEW.md)
- [Grafana trace/profile correlation: provisioning and backend validation](otel/CORRELATION_REVIEW.md)
- [Explicit signals and metric → trace → profile correlation validation](otel/SIGNALS_REVIEW.md)
- [Linux arm64 profiling: validation on an integrated memory nvidia device, CPU costs, and lifecycle checks](otel/ARM64_PROFILES.md)

Historical correlated-profile measurements are described in the
[tracer guide](../tracer/otel/README.md#performance-and-sampling-cost), with
[initial raw measurements](otel/PROFILE_RESULTS.csv) and
[worker-optimization measurements](otel/PROFILE_OPTIMIZATION_RESULTS.csv).
