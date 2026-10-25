//! Per-signal cleanup results under a single caller deadline.
use crate::{ExportStats, FlushError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DrainReport {
    pub traces: Result<ExportStats, FlushError>,
    pub metrics: Result<(), FlushError>,
    pub logs: Result<(), FlushError>,
    /// `None` when this operation did not request a profile drain.
    /// Completion means local processing/upload queues drained, not server acknowledgement.
    pub profiles: Option<Result<(), FlushError>>,
}
impl DrainReport {
    pub fn result(self) -> Result<ExportStats, FlushError> {
        let stats = self.traces?;
        self.metrics?;
        self.logs?;
        self.profiles.unwrap_or(Ok(()))?;
        Ok(stats)
    }
}

/// Drain traces, metrics and logs, retaining each signal's result.
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
    let profiles = shutdown.then(|| crate::profiles::shutdown(remaining()));
    DrainReport {
        traces,
        metrics,
        logs,
        profiles,
    }
}
