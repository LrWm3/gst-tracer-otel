//! Health snapshots reuse existing counters; detailed timings are opt-in.
use crate::{DrainReport, FlushError};
use gst_mars_metrics_core::{
    monitoring::{self, sample, Monitoring},
    proto,
};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
static CLEANUP: [[AtomicU64; 2]; 4] = [const { [const { AtomicU64::new(0) }; 2] }; 4];

pub(crate) fn configure(level: Monitoring) {
    monitoring::configure(level);
    monitoring::set_extra(snapshot);
}
pub(crate) fn cleanup(report: &DrainReport) {
    if monitoring::level() == Monitoring::None {
        return;
    }
    let results = [
        report.traces.as_ref().map(|_| ()),
        report.metrics.as_ref().copied(),
        report.logs.as_ref().copied(),
        report
            .profiles
            .as_ref()
            .map_or(Ok(()), |r| r.as_ref().copied()),
    ];
    for (signal, result) in results.into_iter().enumerate() {
        if let Err(error) = result {
            CLEANUP[signal][usize::from(*error == FlushError::Timeout)].fetch_add(1, Relaxed);
        }
    }
}
fn snapshot() -> Vec<proto::MetricFamily> {
    let mut families = Vec::new();
    let Ok(runtime) = crate::tracer::runtime() else {
        return families;
    };
    let traces = runtime.stats.snapshot();
    let metrics = runtime.metric_stats();
    let logs = crate::log_stats();
    let profiles = crate::profile_stats();
    sample(
        &mut families,
        "mars_instrument_profile_descriptions",
        "Live and pending-retirement element descriptions; capacity 4096",
        true,
        profiles.context_descriptions,
        &[],
    );
    sample(
        &mut families,
        "mars_instrument_profile_descriptions_retired_total",
        "Element descriptions reclaimed after their final sample drain",
        false,
        profiles.context_retired,
        &[],
    );
    for (signal, capacity) in [
        (
            "traces",
            if runtime.config.traces {
                runtime.config.queue_capacity as u64
            } else {
                0
            },
        ),
        ("logs", logs.capacity),
    ] {
        sample(
            &mut families,
            "mars_instrument_queue_capacity",
            "Configured pending-record queue capacity, excluding in-flight batches",
            true,
            capacity,
            &[("signal", signal)],
        );
    }
    for (reason, count) in [
        ("propagation_miss", traces.propagation_misses),
        ("depth", traces.dropped_depth),
        ("mismatched_hooks", traces.mismatched_hooks),
    ] {
        sample(
            &mut families,
            "mars_instrument_capture_failures_total",
            "Trace capture failures by fixed reason",
            false,
            count,
            &[("reason", reason)],
        );
    }
    for (signal, count) in [
        ("traces", traces.export_warnings),
        ("logs", logs.warnings),
        ("metrics", metrics.export_warnings),
    ] {
        sample(
            &mut families,
            "mars_instrument_export_warnings_total",
            "Collector responses containing warnings",
            false,
            count,
            &[("signal", signal)],
        );
    }
    for (signal, outcomes, pending, failures) in [
        (
            "traces",
            [
                traces.exported_spans,
                traces.rejected_spans,
                traces
                    .failed_export_spans
                    .saturating_sub(traces.rejected_spans),
                traces.dropped_queue_full,
                traces.dropped_closed,
            ],
            traces.pending_spans,
            traces.failed_exports,
        ),
        (
            "logs",
            [
                logs.exported,
                logs.rejected,
                logs.failed.saturating_sub(logs.rejected),
                logs.dropped_queue_full,
                logs.dropped_closed,
            ],
            logs.pending,
            logs.failed_exports,
        ),
    ] {
        for (outcome, count) in ["accepted", "rejected", "failed", "queue_full", "closed"]
            .into_iter()
            .zip(outcomes)
        {
            sample(
                &mut families,
                "mars_instrument_records_total",
                "Terminal delivery outcomes; acceptance does not imply durable storage",
                false,
                count,
                &[("signal", signal), ("outcome", outcome)],
            );
        }
        sample(
            &mut families,
            "mars_instrument_pending_records",
            "Queued, batched and in-flight records",
            true,
            pending,
            &[("signal", signal)],
        );
        sample(
            &mut families,
            "mars_instrument_export_failures_total",
            "Failed export requests including partial rejection",
            false,
            failures,
            &[("signal", signal)],
        );
    }
    sample(
        &mut families,
        "mars_instrument_export_failures_total",
        "Failed export requests including partial rejection",
        false,
        metrics.failed_exports,
        &[("signal", "metrics")],
    );
    sample(
        &mut families,
        "mars_instrument_metric_exports_total",
        "Successful cumulative metric export requests",
        false,
        metrics.exported_batches,
        &[],
    );
    sample(
        &mut families,
        "mars_instrument_export_failures_total",
        "Failed export requests including partial rejection",
        false,
        profiles.failed_exports,
        &[("signal", "profiles")],
    );
    for (reason, count) in [
        ("capacity", metrics.dropped_capacity),
        ("invalid", metrics.dropped_invalid),
    ] {
        sample(
            &mut families,
            "mars_instrument_metric_drops_total",
            "Metric observations rejected by the collector",
            false,
            count,
            &[("reason", reason)],
        );
    }
    for (outcome, count) in [
        ("collected", profiles.samples),
        ("reported", profiles.reported_samples),
        ("accepted", profiles.accepted_samples),
        ("export_failed", profiles.failed_samples),
        ("ring_full", profiles.dropped_full),
        ("processing_drop", profiles.dropped_processing),
        ("upload_queue_drop", profiles.dropped_upload),
    ] {
        sample(
            &mut families,
            "mars_instrument_profile_samples_total",
            "Profile sample counts by stage; accepted means HTTP acknowledgement",
            false,
            count,
            &[("outcome", outcome)],
        );
    }
    for (name, help, count) in [
        (
            "mars_instrument_profile_pending_samples",
            "Samples queued or awaiting HTTP acknowledgement",
            profiles.pending_samples,
        ),
        (
            "mars_instrument_profile_pending_uploads",
            "Profiles queued or awaiting HTTP acknowledgement; at most 11",
            profiles.pending_profiles,
        ),
    ] {
        sample(&mut families, name, help, true, count, &[]);
    }
    sample(
        &mut families,
        "mars_instrument_profile_uploads_total",
        "Acknowledged profile requests",
        false,
        profiles.accepted_profiles,
        &[],
    );
    for (reason, count) in [
        ("registration", profiles.registration_failures),
        ("export", profiles.failed_exports),
        ("attribution_capacity", profiles.context_capacity_rejections),
        ("stack_read", profiles.failed_stack_reads),
        ("timer_overrun", profiles.timer_overruns),
    ] {
        sample(
            &mut families,
            "mars_instrument_profile_failures_total",
            "Profiler failures by fixed reason",
            false,
            count,
            &[("reason", reason)],
        );
    }
    sample(
        &mut families,
        "mars_instrument_profile_threads",
        "Threads currently registered with the CPU sampler",
        true,
        profiles.active_threads,
        &[],
    );
    for (i, signal) in ["traces", "metrics", "logs", "profiles"].iter().enumerate() {
        for (j, reason) in ["failure", "timeout"].iter().enumerate() {
            sample(
                &mut families,
                "mars_instrument_cleanup_failures_total",
                "Failed cleanup calls; retries are separate observations",
                false,
                CLEANUP[i][j].load(Relaxed),
                &[("signal", signal), ("reason", reason)],
            );
        }
    }
    if monitoring::level() == Monitoring::All {
        for (component, counter, value) in [
            ("traces", "started", traces.started_spans),
            ("traces", "completed", traces.completed_spans),
            ("traces", "enqueued", traces.enqueued_spans),
            ("traces", "unsampled_pushes", traces.unsampled_pushes),
            ("traces", "unsampled_pulls", traces.unsampled_pulls),
            ("traces", "unsampled_lists", traces.unsampled_lists),
            (
                "traces",
                "propagation_fallbacks",
                traces.propagation_fallbacks,
            ),
            ("metrics", "recorded_pushes", metrics.recorded_pushes),
            ("metrics", "skipped_exemplars", metrics.skipped_exemplars),
            (
                "profiles",
                "correlated_samples",
                profiles.correlated_samples,
            ),
            ("profiles", "element_samples", profiles.element_samples),
            ("profiles", "task_samples", profiles.task_samples),
            ("profiles", "truncated_stacks", profiles.truncated_stacks),
        ] {
            sample(
                &mut families,
                "mars_instrument_diagnostics_total",
                "Detailed instrumentation counters",
                false,
                value,
                &[("component", component), ("counter", counter)],
            );
        }
    }
    families
}
