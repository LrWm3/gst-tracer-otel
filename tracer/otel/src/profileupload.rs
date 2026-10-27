//! Bounded Pyroscope delivery with explicit server acknowledgements.
//! Only the upload thread compresses or performs HTTP. A drain never retries
//! an ambiguous request; applications can use a forwarding proxy for retries.
use crate::export::FlushError;
use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};
use prost::Message;
use pyroscope::{
    backend::{ReportBatch, ReportData},
    encode::gen::{
        push::{PushRequest, PushResponse, RawProfileSeries, RawSample},
        types::LabelPair,
    },
    pyroscope::PyroscopeConfig,
};
use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub(crate) const CAPACITY: usize = 10;
const MAX_REQUEST: usize = 16 * 1024 * 1024;
const MAX_RESPONSE: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Counters {
    pub reported_samples: AtomicU64,
    pub dropped_upload: AtomicU64,
    pub accepted_samples: AtomicU64,
    pub failed_samples: AtomicU64,
    pub reported_profiles: AtomicU64,
    pub accepted_profiles: AtomicU64,
    pub failed_exports: AtomicU64,
}
struct Upload {
    batch: ReportBatch,
    samples: u64,
}
enum Command {
    Upload(Upload),
    Flush(SyncSender<Result<(), FlushError>>),
}
pub(crate) struct Uploader {
    tx: SyncSender<Command>,
    handle: JoinHandle<Result<(), FlushError>>,
    counters: Arc<Counters>,
}
impl Uploader {
    // Construct on the report thread, including the blocking HTTP client.
    pub fn start(
        config: PyroscopeConfig,
        timeout: Duration,
        counters: Arc<Counters>,
    ) -> Result<Self, FlushError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| FlushError::ExportFailed)?;
        let mut url = reqwest::Url::parse(&config.url).map_err(|_| FlushError::ExportFailed)?;
        url.path_segments_mut()
            .map_err(|_| FlushError::ExportFailed)?
            .pop_if_empty()
            .push("push.v1.PusherService")
            .push("Push");
        let (tx, rx) = sync_channel::<Command>(CAPACITY);
        let state = counters.clone();
        let handle = std::thread::Builder::new()
            .name("mars-profile-tx".into())
            .spawn(move || {
                let _suppression = opentelemetry::Context::enter_telemetry_suppressed_scope();
                let mut failed = false;
                while let Ok(command) = rx.recv() {
                    let upload = match command {
                        Command::Upload(upload) => upload,
                        Command::Flush(reply) => {
                            // FIFO barrier: all earlier uploads are acknowledged
                            // or failed. Start a new error epoch without losing
                            // the HTTP connection pool or cumulative counters.
                            let result = if std::mem::take(&mut failed) {
                                Err(FlushError::ExportFailed)
                            } else {
                                Ok(())
                            };
                            let _ = reply.send(result);
                            continue;
                        }
                    };
                    let result = send(&client, &url, &config, upload.batch);
                    if result.is_ok() {
                        state.accepted_samples.fetch_add(upload.samples, Relaxed);
                        state.accepted_profiles.fetch_add(1, Relaxed);
                    } else {
                        state.failed_samples.fetch_add(upload.samples, Relaxed);
                        state.failed_exports.fetch_add(1, Relaxed);
                        failed = true;
                    }
                }
                if failed {
                    Err(FlushError::ExportFailed)
                } else {
                    Ok(())
                }
            })
            .map_err(|_| FlushError::ExportFailed)?;
        Ok(Self {
            tx,
            handle,
            counters,
        })
    }
    pub fn submit(&self, batch: ReportBatch, samples: u64, drain: bool) -> Result<(), FlushError> {
        let upload = Command::Upload(Upload { batch, samples });
        let result = if drain {
            self.tx
                .send(upload)
                .map_err(|e| TrySendError::Disconnected(e.0))
        } else {
            self.tx.try_send(upload)
        };
        match result {
            Ok(()) => {
                self.counters.reported_samples.fetch_add(samples, Relaxed);
                self.counters.reported_profiles.fetch_add(1, Relaxed);
                Ok(())
            }
            Err(error) => {
                self.counters.dropped_upload.fetch_add(samples, Relaxed);
                match error {
                    TrySendError::Full(_) => Ok(()),
                    TrySendError::Disconnected(_) => Err(FlushError::ExportFailed),
                }
            }
        }
    }
    // Only report workers wait here. Public callers retain their own deadlines.
    pub fn flush(&self) -> Result<(), FlushError> {
        let (reply, result) = sync_channel(1);
        self.tx
            .send(Command::Flush(reply))
            .map_err(|_| FlushError::ExportFailed)?;
        result.recv().map_err(|_| FlushError::ExportFailed)?
    }
    pub fn finish(self) -> Result<(), FlushError> {
        drop(self.tx);
        self.handle.join().map_err(|_| FlushError::ExportFailed)?
    }
}

fn send(
    client: &reqwest::blocking::Client,
    url: &reqwest::Url,
    config: &PyroscopeConfig,
    batch: ReportBatch,
) -> Result<(), FlushError> {
    let ReportData::RawPprof(raw_profile) = batch.data else {
        return Err(FlushError::ExportFailed);
    };
    let _work = gst_mars_metrics_core::monitoring::work(
        gst_mars_metrics_core::monitoring::Stage::ProfileUpload,
        raw_profile.len(),
        1,
    );
    let mut labels: Vec<LabelPair> = [
        ("__name__", batch.profile_type.as_str()),
        ("service_name", config.application_name.as_str()),
        ("otel.scope.name", "mars-otel"),
        ("otel.scope.version", config.spy_version.as_str()),
        ("process.runtime.name", "rust"),
    ]
    .into_iter()
    .map(|(name, value)| LabelPair {
        name: name.into(),
        value: value.into(),
    })
    .collect();
    // GPU reports need a stable per-profiler series identity as well as distinct
    // timestamps: several processes can submit the same service/bucket. This
    // identity survives explicit flushes; CPU series are unchanged.
    if let Some(instance) = config.tags.get("service_instance_id") {
        labels.push(LabelPair {
            name: "service_instance_id".into(),
            value: instance.clone(),
        });
    }
    // A fresh 128-bit profile ID preserves the existing Pyroscope push shape.
    let id = RandomIdGenerator::default().new_trace_id().to_string();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &id[..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..]
    );
    let request = PushRequest {
        series: vec![RawProfileSeries {
            labels,
            samples: vec![RawSample { raw_profile, id }],
        }],
    };
    if request.encoded_len() > MAX_REQUEST {
        return Err(FlushError::ExportFailed);
    }
    let mut gzip =
        libflate::gzip::Encoder::new(Vec::new()).map_err(|_| FlushError::ExportFailed)?;
    gzip.write_all(&request.encode_to_vec())
        .map_err(|_| FlushError::ExportFailed)?;
    let body = gzip
        .finish()
        .into_result()
        .map_err(|_| FlushError::ExportFailed)?;
    let response = client
        .post(url.clone())
        .header(
            "User-Agent",
            concat!("mars-otel/", env!("CARGO_PKG_VERSION")),
        )
        .header("Content-Type", "application/proto")
        .header("Content-Encoding", "gzip")
        .body(body)
        .send()
        .map_err(|_| FlushError::ExportFailed)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(FlushError::ExportFailed);
    }
    // Pyroscope can return gzip even when the client did not advertise it.
    // Bound both wire bytes and inflated bytes before decoding protobuf.
    let gzip = match response.headers().get(reqwest::header::CONTENT_ENCODING) {
        None => false,
        Some(value) if value == "identity" => false,
        Some(value) if value == "gzip" => true,
        Some(_) => return Err(FlushError::ExportFailed),
    };
    let mut body = Vec::new();
    let length = response
        .take(MAX_RESPONSE as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| FlushError::ExportFailed)?;
    if length > MAX_RESPONSE {
        return Err(FlushError::ExportFailed);
    }
    if gzip {
        let mut decoded = Vec::new();
        let length = libflate::gzip::Decoder::new(body.as_slice())
            .map_err(|_| FlushError::ExportFailed)?
            .take(MAX_RESPONSE as u64 + 1)
            .read_to_end(&mut decoded)
            .map_err(|_| FlushError::ExportFailed)?;
        if length > MAX_RESPONSE {
            return Err(FlushError::ExportFailed);
        }
        body = decoded;
    }
    PushResponse::decode(body.as_slice()).map_err(|_| FlushError::ExportFailed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> ReportBatch {
        ReportBatch {
            profile_type: "process_cpu".into(),
            data: ReportData::RawPprof(vec![1, 2, 3]),
        }
    }
    fn config(url: String) -> PyroscopeConfig {
        PyroscopeConfig::new(url, "delivery-test", 100, "mars-otel", "test")
    }
    fn decode(request: &mut tiny_http::Request) {
        assert_eq!(request.url(), "/prefix/push.v1.PusherService/Push");
        assert_eq!(
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Encoding"))
                .unwrap()
                .value
                .as_str(),
            "gzip"
        );
        let mut compressed = Vec::new();
        request.as_reader().read_to_end(&mut compressed).unwrap();
        let mut data = Vec::new();
        libflate::gzip::Decoder::new(compressed.as_slice())
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        let request = PushRequest::decode(data.as_slice()).unwrap();
        assert_eq!(request.series.len(), 1);
        let series = &request.series[0];
        assert!(series
            .labels
            .iter()
            .any(|l| l.name == "service_name" && l.value == "delivery-test"));
        assert!(series
            .labels
            .iter()
            .any(|l| l.name == "__name__" && l.value == "process_cpu"));
        assert_eq!(series.samples[0].raw_profile, [1, 2, 3]);
        assert_eq!(series.samples[0].id.len(), 36);
    }

    #[test]
    fn flush_is_a_fifo_barrier_and_delivery_recovers_without_restarting_the_worker() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", server.server_addr());
        let counters = Arc::new(Counters::default());
        let uploader =
            Uploader::start(config(url), Duration::from_secs(3), counters.clone()).unwrap();
        let worker_id = uploader.handle.thread().id();
        let receiver = std::thread::spawn(move || {
            for status in [503, 200] {
                let mut request = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                decode(&mut request);
                request.respond(tiny_http::Response::empty(status)).unwrap();
            }
            assert!(server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none());
        });
        uploader.submit(batch(), 7, true).unwrap();
        assert_eq!(uploader.flush(), Err(FlushError::ExportFailed));
        assert_eq!(counters.failed_samples.load(Relaxed), 7);
        assert_eq!(uploader.flush(), Ok(()));
        uploader.submit(batch(), 11, true).unwrap();
        assert_eq!(uploader.flush(), Ok(()));
        assert_eq!(counters.accepted_samples.load(Relaxed), 11);
        assert_eq!(counters.reported_profiles.load(Relaxed), 2);
        assert_eq!(uploader.handle.thread().id(), worker_id);
        assert_eq!(uploader.finish(), Ok(()));
        receiver.join().unwrap();
    }

    #[test]
    fn flush_waits_for_prior_delivery_without_sending_an_extra_request() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", server.server_addr());
        let counters = Arc::new(Counters::default());
        let uploader =
            Uploader::start(config(url), Duration::from_secs(3), counters.clone()).unwrap();
        uploader.submit(batch(), 5, true).unwrap();
        let mut request = server
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        decode(&mut request);
        let (done, completed) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = uploader.flush();
            done.send(result).unwrap();
            uploader.finish()
        });
        assert!(completed.recv_timeout(Duration::from_millis(50)).is_err());
        request.respond(tiny_http::Response::empty(200)).unwrap();
        assert_eq!(
            completed.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        assert_eq!(counters.accepted_samples.load(Relaxed), 5);
        assert!(server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none());
        assert_eq!(worker.join().unwrap(), Ok(()));
    }

    #[test]
    fn acknowledgements_rejections_and_bad_responses_are_accounted_without_retries() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix/", server.server_addr());
        let worker = std::thread::spawn(move || {
            for (status, body) in [
                (200, vec![]),
                (503, vec![]),
                (429, vec![]),
                (200, vec![0xff]),
                (200, vec![0; MAX_RESPONSE + 1]),
                (200, vec![]),
            ] {
                let mut request = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                decode(&mut request);
                let _ =
                    request.respond(tiny_http::Response::from_data(body).with_status_code(status));
            }
            assert!(server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none());
        });
        let counters = Arc::new(Counters::default());
        let uploader =
            Uploader::start(config(url), Duration::from_secs(2), counters.clone()).unwrap();
        for _ in 0..6 {
            uploader.submit(batch(), 7, true).unwrap();
        }
        assert_eq!(uploader.finish(), Err(FlushError::ExportFailed));
        worker.join().unwrap();
        assert_eq!(counters.reported_samples.load(Relaxed), 42);
        assert_eq!(counters.accepted_samples.load(Relaxed), 14);
        assert_eq!(counters.failed_samples.load(Relaxed), 28);
        assert_eq!(counters.accepted_profiles.load(Relaxed), 2);
        assert_eq!(counters.failed_exports.load(Relaxed), 4);
        assert_eq!(counters.dropped_upload.load(Relaxed), 0);
    }

    #[test]
    fn compressed_acknowledgements_are_decoded_with_an_inflated_size_limit() {
        fn gzip(bytes: &[u8]) -> Vec<u8> {
            let mut writer = libflate::gzip::Encoder::new(Vec::new()).unwrap();
            writer.write_all(bytes).unwrap();
            writer.finish().into_result().unwrap()
        }
        // A valid unknown protobuf field would otherwise be accepted. Its
        // compressed representation is small but its decoded size is too large.
        let mut oversized = vec![0x9a, 0x06];
        prost::encoding::encode_varint((MAX_RESPONSE * 2) as u64, &mut oversized);
        oversized.resize(oversized.len() + MAX_RESPONSE * 2, b'x');
        assert!(PushResponse::decode(oversized.as_slice()).is_ok());
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", server.server_addr());
        let worker = std::thread::spawn(move || {
            for (body, encoding) in [
                (gzip(&[]), "gzip"),
                (gzip(&oversized), "gzip"),
                (vec![0xff], "gzip"),
                (vec![], "br"),
            ] {
                let mut request = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                decode(&mut request);
                let _ = request.respond(tiny_http::Response::from_data(body).with_header(
                    tiny_http::Header::from_bytes("Content-Encoding", encoding).unwrap(),
                ));
            }
        });
        let counters = Arc::new(Counters::default());
        let uploader =
            Uploader::start(config(url), Duration::from_secs(2), counters.clone()).unwrap();
        for _ in 0..4 {
            uploader.submit(batch(), 1, true).unwrap();
        }
        assert_eq!(uploader.finish(), Err(FlushError::ExportFailed));
        worker.join().unwrap();
        assert_eq!(counters.accepted_samples.load(Relaxed), 1);
        assert_eq!(counters.failed_samples.load(Relaxed), 3);
    }

    #[test]
    fn timeout_is_a_delivery_failure_and_queue_pressure_never_blocks_reporting() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", server.server_addr());
        let counters = Arc::new(Counters::default());
        let uploader =
            Uploader::start(config(url), Duration::from_secs(2), counters.clone()).unwrap();
        uploader.submit(batch(), 3, false).unwrap();
        // Receiving the request proves the worker has removed it from the queue.
        let mut first = server
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        decode(&mut first);
        for _ in 0..CAPACITY {
            uploader.submit(batch(), 5, false).unwrap();
        }
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            uploader.submit(batch(), 7, false).unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            counters.reported_profiles.load(Relaxed),
            CAPACITY as u64 + 1
        );
        assert_eq!(counters.dropped_upload.load(Relaxed), 7000);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2100));
            let _ = first.respond(tiny_http::Response::empty(200));
            for _ in 0..CAPACITY {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                decode(&mut request);
                let _ = request.respond(tiny_http::Response::empty(200));
            }
        });
        assert_eq!(uploader.finish(), Err(FlushError::ExportFailed));
        worker.join().unwrap();
        assert!(counters.failed_samples.load(Relaxed) >= 3);
        assert_eq!(
            counters.accepted_samples.load(Relaxed) + counters.failed_samples.load(Relaxed),
            53
        );
    }
}
