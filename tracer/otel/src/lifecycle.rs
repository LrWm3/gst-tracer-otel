//! Per-signal cleanup results under a single caller deadline.
use crate::{ExportStats, FlushError};
use glib::prelude::*;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DrainReport {
    pub traces: Result<ExportStats, FlushError>,
    pub metrics: Result<(), FlushError>,
    pub logs: Result<(), FlushError>,
    /// `None` when this operation did not request a profile drain.
    /// Completion includes HTTP acknowledgement; failures since the previous
    /// profile drain are returned even if subsequent requests succeeded.
    pub profiles: Option<Result<(), FlushError>>,
    /// Every profile queued through this drain was acknowledged, with no
    /// processing or upload loss. This does not promise durable backend storage.
    pub profile_delivery_confirmed: bool,
}
impl DrainReport {
    pub fn to_structure(&self) -> gstreamer::Structure {
        fn status<T>(result: &Result<T, FlushError>) -> &'static str {
            match result {
                Ok(_) => "ok",
                Err(FlushError::Timeout) => "timeout",
                Err(FlushError::ExportFailed) => "export-failed",
                Err(FlushError::NotInitialized) => "not-initialized",
                Err(FlushError::Closed) => "closed",
            }
        }
        gstreamer::Structure::builder("mars-drain-report")
            .field("success", self.clone().result().is_ok())
            .field("traces", status(&self.traces))
            .field("metrics", status(&self.metrics))
            .field("logs", status(&self.logs))
            .field(
                "profiles",
                self.profiles.as_ref().map_or("not-requested", status),
            )
            .field(
                "profile-delivery-confirmed",
                self.profile_delivery_confirmed,
            )
            .build()
    }
    pub fn result(self) -> Result<ExportStats, FlushError> {
        let stats = self.traces?;
        self.metrics?;
        self.logs?;
        self.profiles.unwrap_or(Ok(()))?;
        Ok(stats)
    }
}

/// Drain every signal, retaining each result. Sampling continues after a flush.
pub fn flush_report(timeout: Duration) -> DrainReport {
    drain(timeout, false)
}
/// Shut down every signal, retaining each result even if another fails.
pub fn shutdown_report(timeout: Duration) -> DrainReport {
    drain(timeout, true)
}
fn drain(timeout: Duration, shutdown: bool) -> DrainReport {
    let start = Instant::now();
    let (traces, metrics) = match crate::tracer::runtime() {
        Ok(runtime) if shutdown => runtime.shutdown_parts(timeout),
        Ok(runtime) => runtime.flush_parts(timeout),
        Err(error) => (Err(error.clone()), Err(error)),
    };
    let remaining = || timeout.saturating_sub(start.elapsed());
    let logs = if shutdown {
        crate::otellogbridge::shutdown(remaining())
    } else {
        crate::otellogbridge::flush(remaining())
    };
    let profiles = Some(if shutdown {
        crate::profiles::shutdown(remaining())
    } else {
        crate::profiles::flush(remaining())
    });
    let stats = crate::profile_stats();
    let cpu_delivery_complete = stats.reported_samples == stats.accepted_samples
        && stats.failed_samples == 0
        && stats.dropped_processing == 0
        && stats.dropped_upload == 0;
    #[cfg(not(all(feature = "gpu-profiles", target_os = "linux")))]
    let profile_delivery_confirmed = profiles.as_ref().is_some_and(Result::is_ok)
        && stats.reported_samples > 0
        && cpu_delivery_complete;
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    let profile_delivery_confirmed = {
        let gpu = crate::gpu_profile_stats();
        profiles.as_ref().is_some_and(Result::is_ok)
            && (stats.reported_samples > 0 || gpu.reported_kernels > 0)
            && cpu_delivery_complete
            && gpu.reported_kernels == gpu.accepted_kernels
            && gpu.failed_kernels + gpu.dropped_processing + gpu.dropped_upload == 0
    };
    let report = DrainReport {
        traces,
        metrics,
        logs,
        profiles,
        profile_delivery_confirmed,
    };
    crate::health::cleanup(&report);
    report
}

pub(crate) fn signals() -> Vec<glib::subclass::Signal> {
    ["flush", "shutdown"]
        .into_iter()
        .map(|name| {
            glib::subclass::Signal::builder(name)
                .param_types([u64::static_type()])
                .return_type::<gstreamer::Structure>()
                .action()
                .class_handler(move |args| {
                    let ms = args[1].get::<u64>().unwrap();
                    let report = if ms > 60000 {
                        gstreamer::Structure::builder("mars-drain-report")
                            .field("success", false)
                            .field("error", "timeout must be 0..=60000 milliseconds")
                            .build()
                    } else {
                        drain(Duration::from_millis(ms), name == "shutdown").to_structure()
                    };
                    Some(report.to_value())
                })
                .build()
        })
        .collect()
}
