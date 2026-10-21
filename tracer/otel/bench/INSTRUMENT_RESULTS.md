# Initial gst-instrument measurements

Measured 2026-10-21 on Linux x86-64, Intel Core i5-1345U, stock GStreamer 1.24.2,
Rust 1.89.0 release build. Five repetitions with rotated mode ordering; no CPU
affinity or frequency pinning. Results are short-run diagnostics, not production
throughput or tail-latency guarantees.

Each run processes 10,000 buffers through four identity elements and a fakesink:
50,000 push calls, divided across one or four source threads. CPU histograms
sample approximately 1/16 calls; wall histograms record every call. Profiles and
logs are off. Trace queues hold 65,536 records to avoid dropping this workload.
A local HTTP receiver stores payloads and decodes them after execution. Metric
snapshots are drained at pipeline NULL, outside the pipeline wall-time interval.

Medians below. Source CPU is summed across streaming source threads; process
CPU includes the exporter and final drain. RSS comes from `/proc/self/status`
`VmHWM`, excluding the Python launcher’s inherited pre-exec high-water mark.

| Sources | Mode | Source CPU (ms) | Process CPU (ms) | Peak RSS (MiB) |
| --- | --- | ---: | ---: | ---: |
| 1 | No tracer | 15.8 | 16.4 | 8.8 |
| 1 | Traces only (`otel-tracer`) | 56.5 | 98.4 | 22.4 |
| 1 | Metrics only | 36.3 | 37.8 | 13.7 |
| 1 | Metrics + traces | 47.8 | 90.2 | 22.8 |
| 1 | Metrics + unsampled trace path | 35.1 | 36.7 | 22.1 |
| 4 | No tracer | 30.2 | 32.1 | 8.8 |
| 4 | Traces only (`otel-tracer`) | 67.3 | 110.8 | 22.6 |
| 4 | Metrics only | 58.0 | 61.0 | 14.1 |
| 4 | Metrics + traces | 76.2 | 119.7 | 23.2 |
| 4 | Metrics + unsampled trace path | 64.8 | 67.7 | 22.5 |

For four sources, adding metrics to traces increased median source CPU from
67.3 to 76.2 ms: about 13%, or 0.18 microseconds per push. Median process CPU
increased about 8%, and peak RSS about 0.6 MiB. Metrics-only operation used
about 14 MiB peak RSS and generated no trace traffic.

Single-source measurements were noisy: trace-only source CPU ranged from
34.1–62.1 ms, and combined capture from 40.3–78.2 ms. The lower combined median
does not establish a speedup. These ranges overlap; longer controlled runs are
needed to estimate small differences reliably.

All 50 runs passed the decoded-output checks: 1,000,000 expected spans and
1,500,000 cumulative metric observations arrived across the applicable modes.
The benchmark asserts exact counts and checks metric failure counters. Integration
tests separately force trace queue drops, partial metric rejection, slow receivers,
and a series-capacity limit; these are not included in the timing table.

## Reproduce

```sh
cargo build --release -p gst-otel-tracer
python3 tracer/otel/bench/instrument.py --repeats 5 --buffers 10000 \
  --output tracer/otel/bench/INSTRUMENT_RESULTS.csv
```

The benchmark uses the existing [pipeline driver](pipeline.c). Raw measurements
are in [INSTRUMENT_RESULTS.csv](INSTRUMENT_RESULTS.csv). More buffers may require
a larger trace queue or a faster receiver; the harness rejects missing spans
instead of presenting a reduced export workload as a performance improvement.

The metrics path uses fixed counter shards to avoid a global accounting cache
line shared by every source. Metrics-only mode allocates no trace queue. Series
registration, retirement, export chunking, and exemplars are bounded as described
in the [implementation guide](../INSTRUMENT.md).
