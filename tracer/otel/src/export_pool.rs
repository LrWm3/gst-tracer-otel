//! Optional bounded trace-export parallelism. The capture FIFO still contains
//! flush/shutdown barriers; each barrier waits for every earlier request.
use super::*;

type Completion = (Vec<Record>, Result<(), FlushError>);

pub(super) enum BatchExporter<E> {
    Serial(E),
    Concurrent(Pool),
}

pub(super) struct Pool {
    jobs: Option<Sender<Vec<Record>>>,
    results: Receiver<Completion>,
    workers: Vec<JoinHandle<Result<(), FlushError>>>,
    in_flight: usize,
    free: Vec<Vec<Record>>,
}

impl Pool {
    fn completed(&mut self, (batch, result): Completion) -> Result<(), FlushError> {
        self.in_flight -= 1;
        self.free.push(batch);
        result
    }
    fn wait_one(&mut self) -> Result<(), FlushError> {
        match self.results.recv() {
            Ok(completion) => self.completed(completion),
            Err(_) => {
                self.in_flight = 0;
                Err(FlushError::Closed)
            }
        }
    }
}

impl<E: RecordExporter> BatchExporter<E> {
    pub fn new(
        exporter: E,
        config: &Config,
        stats: Arc<Counters>,
        anchor: Arc<OnceLock<Anchor>>,
        resource: &Resource,
    ) -> std::io::Result<Self> {
        if config.concurrency == 1 {
            return Ok(Self::Serial(exporter));
        }
        let mut exporters = Vec::with_capacity(config.concurrency);
        for _ in 1..config.concurrency {
            let mut fork = exporter.fork(resource)?;
            fork.set_resource(resource);
            exporters.push(fork);
        }
        exporters.push(exporter);
        // Zero waiting jobs: only idle workers accept a batch. Completion
        // storage and reusable vectors are bounded by the worker count.
        let (jobs, input) = bounded::<Vec<Record>>(0);
        let (output, results) = bounded(config.concurrency);
        let mut workers = Vec::with_capacity(config.concurrency);
        for (index, mut exporter) in exporters.into_iter().enumerate() {
            let input = input.clone();
            let output = output.clone();
            let stats = stats.clone();
            let anchor = anchor.clone();
            let config = config.clone();
            workers.push(
                thread::Builder::new()
                    .name(format!("mars-otel-tx{index}"))
                    .spawn(move || {
                        let _suppression =
                            opentelemetry::Context::enter_telemetry_suppressed_scope();
                        let stats = stats.local();
                        let mut poisoned = false;
                        while let Ok(mut batch) = input.recv() {
                            let count = batch.len() as u64;
                            let outcome = if poisoned {
                                None
                            } else {
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    export_batch(&mut exporter, &mut batch, stats, &anchor, &config)
                                }))
                                .ok()
                            };
                            let result = match outcome {
                                Some(result) => result,
                                None => {
                                    // An exporter panic must not strand a barrier while
                                    // the other workers still own completion senders.
                                    poisoned = true;
                                    stats.failed_export_spans.fetch_add(count, Relaxed);
                                    stats.failed_exports.fetch_add(1, Relaxed);
                                    batch.clear();
                                    Err(FlushError::ExportFailed)
                                }
                            };
                            if output.send((batch, result)).is_err() {
                                break;
                            }
                        }
                        if poisoned {
                            Err(FlushError::ExportFailed)
                        } else {
                            exporter.shutdown(config.flush_timeout)
                        }
                    })?,
            );
        }
        Ok(Self::Concurrent(Pool {
            jobs: Some(jobs),
            results,
            workers,
            in_flight: 0,
            free: Vec::new(),
        }))
    }

    pub fn submit(
        &mut self,
        batch: &mut Vec<Record>,
        stats: &LocalCounters,
        anchor: &OnceLock<Anchor>,
        config: &Config,
    ) -> Result<(), FlushError> {
        let Self::Concurrent(pool) = self else {
            let Self::Serial(exporter) = self else {
                unreachable!()
            };
            return export_batch(exporter, batch, stats, anchor, config);
        };
        let mut failed = false;
        while let Ok(completion) = pool.results.try_recv() {
            failed |= pool.completed(completion).is_err();
        }
        if !batch.is_empty() {
            if pool.in_flight == pool.workers.len() {
                failed |= pool.wait_one().is_err();
            }
            let reusable = pool
                .free
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(config.batch_size));
            let outgoing = std::mem::replace(batch, reusable);
            if let Err(error) = pool.jobs.as_ref().unwrap().send(outgoing) {
                stats
                    .failed_export_spans
                    .fetch_add(error.0.len() as u64, Relaxed);
                stats.failed_exports.fetch_add(1, Relaxed);
                failed = true;
            } else {
                pool.in_flight += 1;
            }
        }
        if failed {
            Err(FlushError::ExportFailed)
        } else {
            Ok(())
        }
    }

    pub fn drain(&mut self) -> Result<(), FlushError> {
        let Self::Concurrent(pool) = self else {
            return Ok(());
        };
        let mut failed = false;
        while pool.in_flight != 0 {
            failed |= pool.wait_one().is_err();
        }
        if failed {
            Err(FlushError::ExportFailed)
        } else {
            Ok(())
        }
    }

    pub fn shutdown(&mut self, timeout: Duration) -> Result<(), FlushError> {
        let mut failed = self.drain().is_err();
        match self {
            Self::Serial(exporter) => failed |= exporter.shutdown(timeout).is_err(),
            Self::Concurrent(pool) => {
                drop(pool.jobs.take());
                for worker in pool.workers.drain(..) {
                    failed |= !matches!(worker.join(), Ok(Ok(())));
                }
            }
        }
        if failed {
            Err(FlushError::ExportFailed)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::tests::record;

    #[derive(Clone)]
    struct Gated {
        entered: Sender<usize>,
        release: Arc<Vec<Receiver<()>>>,
        errors: bool,
    }
    impl RecordExporter for Gated {
        fn fork(&self, _: &Resource) -> std::io::Result<Self> {
            Ok(self.clone())
        }
        fn export(
            &mut self,
            records: &[Record],
            _: &Anchor,
            _: &Config,
        ) -> Result<ExportOutcome, FlushError> {
            let index = u64::from_be_bytes(records[0].token.span_id.to_bytes()) as usize - 1;
            self.entered.send(index).unwrap();
            self.release[index].recv().unwrap();
            if self.errors && index == 0 {
                panic!("simulated exporter panic");
            }
            Ok(ExportOutcome {
                rejected: u64::from(self.errors && index == 1),
                warned: self.errors && index == 1,
            })
        }
    }

    #[test]
    fn parallel_barriers_wait_for_all_requests_and_shutdown_keeps_its_deadline() {
        for errors in [false, true] {
            let channels: Vec<_> = (0..4).map(|_| bounded(1)).collect();
            let (entered, observed) = bounded(4);
            let runtime = Arc::new(
                Runtime::new(
                    Config {
                        batch_size: 1,
                        concurrency: 4,
                        interval: Duration::from_secs(60),
                        ..Default::default()
                    },
                    Gated {
                        entered,
                        release: Arc::new(channels.iter().map(|(_, rx)| rx.clone()).collect()),
                        errors,
                    },
                    Resource::builder_empty().build(),
                )
                .unwrap(),
            );
            runtime.anchor(100);
            for index in 0..4 {
                assert!(runtime.submit(record(index)));
            }
            let mut ids: Vec<_> = (0..4)
                .map(|_| observed.recv_timeout(Duration::from_secs(5)).unwrap())
                .collect();
            ids.sort_unstable();
            assert_eq!(
                ids,
                [0, 1, 2, 3],
                "all four requests must be independently in flight"
            );
            let start = Instant::now();
            assert_eq!(
                runtime.flush(Duration::from_millis(10)),
                Err(FlushError::Timeout)
            );
            assert!(start.elapsed() < Duration::from_secs(1));
            // Finish requests out of order, leaving the earliest blocked.
            for index in [3, 1, 2] {
                channels[index].0.send(()).unwrap();
            }
            assert_eq!(
                runtime.shutdown(Duration::from_millis(10)),
                Err(FlushError::Timeout)
            );
            assert_eq!(
                runtime.shutdown(Duration::from_millis(10)),
                Err(FlushError::Timeout)
            );
            channels[0].0.send(()).unwrap();
            let result = runtime.shutdown(Duration::from_secs(5));
            if errors {
                assert_eq!(result, Err(FlushError::ExportFailed));
            } else {
                result.unwrap();
            }
            let stats = runtime.stats.snapshot();
            assert_eq!(stats.pending_spans, 0);
            assert_eq!(stats.enqueued_spans, 4);
            assert_eq!(stats.exported_spans, if errors { 2 } else { 4 });
            assert_eq!(stats.failed_export_spans, if errors { 2 } else { 0 });
            assert_eq!(stats.rejected_spans, u64::from(errors));
            assert_eq!(stats.dropped_queue_full + stats.dropped_closed, 0);
        }
    }
}
