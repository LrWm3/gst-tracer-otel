//! A single bounded OTLP/HTTP request at a time, with reference-counted bodies.
//! Logs continue to use the upstream exporter. Trace encoding is in wire.rs.
use crate::{
    capture::Record,
    export::{Anchor, Config, FlushError, RecordExporter},
    wire::Encoder,
};
use bytes::Bytes;
use opentelemetry_sdk::Resource;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE},
    Url,
};
use std::time::Duration;

const MAX_RESPONSE: usize = 4 * 1024 * 1024;
#[derive(Debug)]
struct Settings {
    endpoint: Url,
    headers: HeaderMap,
    timeout: Duration,
    asynchronous: bool,
}
impl Settings {
    fn read(signal: &str, get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let setting = |suffix: &str| {
            get(&format!("OTEL_EXPORTER_OTLP_{signal}_{suffix}"))
                .or_else(|| get(&format!("OTEL_EXPORTER_OTLP_{suffix}")))
        };
        if setting("PROTOCOL").is_some_and(|p| !p.is_empty() && p != "http/protobuf") {
            return Err("OTLP export requires http/protobuf".into());
        }
        if setting("COMPRESSION").is_some_and(|p| !p.is_empty() && p != "none") {
            return Err("OTLP compression is not enabled in this build".into());
        }
        let endpoint = if let Some(url) =
            get(&format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")).filter(|s| !s.is_empty())
        {
            Url::parse(&url).map_err(|_| "invalid OTLP endpoint")?
        } else {
            let base = get("OTEL_EXPORTER_OTLP_ENDPOINT")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "http://localhost:4318".into());
            let mut url = Url::parse(&base).map_err(|_| "invalid OTLP endpoint")?;
            url.set_path(&format!(
                "{}/v1/{}",
                url.path().trim_end_matches('/'),
                signal.to_ascii_lowercase()
            ));
            url
        };
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err("OTLP endpoint requires an HTTP(S) host".into());
        }
        let timeout = get(&format!("OTEL_EXPORTER_OTLP_{signal}_TIMEOUT"))
            .and_then(|s| s.parse().ok())
            .or_else(|| get("OTEL_EXPORTER_OTLP_TIMEOUT").and_then(|s| s.parse().ok()))
            .unwrap_or(10_000);
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(concat!("mars-otel/", env!("CARGO_PKG_VERSION"))),
        );
        if let Some(input) = setting("HEADERS") {
            for item in input.split_terminator(',') {
                let Some((key, value)) = item.trim().split_once('=') else {
                    continue;
                };
                let Ok(key) = HeaderName::from_bytes(key.trim().as_bytes()) else {
                    continue;
                };
                let Ok(value) = percent_encoding::percent_decode_str(value.trim()).decode_utf8()
                else {
                    continue;
                };
                let Ok(value) = HeaderValue::from_str(&value) else {
                    continue;
                };
                headers.insert(key, value);
            }
        }
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-protobuf"),
        );
        let asynchronous = match get("GST_OTEL_HTTP_CLIENT").as_deref() {
            None | Some("blocking") => false,
            Some("async") => true,
            Some(_) => return Err("GST_OTEL_HTTP_CLIENT must be blocking or async".into()),
        };
        Ok(Self {
            endpoint,
            headers,
            timeout: Duration::from_millis(timeout),
            asynchronous,
        })
    }
}
enum Client {
    Blocking(reqwest::blocking::Client),
    Async {
        client: reqwest::Client,
        runtime: tokio::runtime::Runtime,
    },
}
pub(crate) struct HttpExporter {
    client: Client,
    settings: Settings,
}
impl HttpExporter {
    pub fn new(signal: &str) -> Result<Self, String> {
        Self::with_settings(Settings::read(signal, |key| std::env::var(key).ok())?)
    }
    fn with_settings(settings: Settings) -> Result<Self, String> {
        let client = if settings.asynchronous {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            Client::Async {
                client: reqwest::Client::builder()
                    .timeout(settings.timeout)
                    .build()
                    .map_err(|e| e.to_string())?,
                runtime,
            }
        } else {
            // Safe even if the application initializes GStreamer inside Tokio.
            let timeout = settings.timeout;
            let client = std::thread::spawn(move || {
                reqwest::blocking::Client::builder()
                    .timeout(timeout)
                    .build()
            })
            .join()
            .map_err(|_| "HTTP client initialization panicked")?
            .map_err(|e| e.to_string())?;
            Client::Blocking(client)
        };
        Ok(Self { client, settings })
    }
    pub fn send(&self, body: Bytes) -> Result<(), FlushError> {
        self.request(body, false).map(|_| ())
    }
    pub fn send_response(&self, body: Bytes) -> Result<Vec<u8>, FlushError> {
        self.request(body, true)
    }
    fn request(&self, body: Bytes, retain_response: bool) -> Result<Vec<u8>, FlushError> {
        let failed = |_| FlushError::ExportFailed;
        match &self.client {
            Client::Blocking(client) => {
                use std::io::Read;
                let response = client
                    .post(self.settings.endpoint.clone())
                    .headers(self.settings.headers.clone())
                    .body(body)
                    .send()
                    .map_err(failed)?;
                let status = response.status();
                let mut limited = response.take(MAX_RESPONSE as u64 + 1);
                let mut response = Vec::new();
                let count = if retain_response {
                    limited.read_to_end(&mut response).map(|n| n as u64)
                } else {
                    std::io::copy(&mut limited, &mut std::io::sink())
                }
                .map_err(|_| FlushError::ExportFailed)?;
                if !status.is_success() || count > MAX_RESPONSE as u64 {
                    return Err(FlushError::ExportFailed);
                }
                Ok(response)
            }
            Client::Async { client, runtime } => runtime.block_on(async {
                let mut response = client
                    .post(self.settings.endpoint.clone())
                    .headers(self.settings.headers.clone())
                    .body(body)
                    .send()
                    .await
                    .map_err(failed)?;
                let status = response.status();
                let mut bytes = Vec::new();
                let mut length = 0;
                while let Some(chunk) = response.chunk().await.map_err(failed)? {
                    length += chunk.len();
                    if length > MAX_RESPONSE {
                        return Err(FlushError::ExportFailed);
                    }
                    if retain_response {
                        bytes.extend_from_slice(&chunk);
                    }
                }
                if status.is_success() {
                    Ok(bytes)
                } else {
                    Err(FlushError::ExportFailed)
                }
            }),
        }
    }
}
pub(crate) struct OtlpExporter {
    http: HttpExporter,
    encoder: Encoder,
}
impl OtlpExporter {
    pub fn new(resource: &Resource) -> Result<Self, String> {
        Ok(Self {
            http: HttpExporter::new("TRACES")?,
            encoder: Encoder::new(resource),
        })
    }
}
impl RecordExporter for OtlpExporter {
    fn export(
        &mut self,
        records: &[Record],
        anchor: &Anchor,
        config: &Config,
    ) -> Result<(), FlushError> {
        self.encoder.encode(records, anchor, config)?;
        let body = std::mem::take(&mut self.encoder.body).freeze();
        let result = self.http.send(body.clone());
        // The request has completed; normally this recovers the same allocation.
        self.encoder.body = body.try_into_mut().unwrap_or_default();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings(values: &[(&str, &str)]) -> Result<Settings, String> {
        Settings::read("TRACES", |key| {
            values
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    }
    #[test]
    fn endpoint_headers_timeout_and_transport_follow_signal_precedence() {
        let base = settings(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://host/prefix/")]).unwrap();
        assert_eq!(base.endpoint.as_str(), "https://host/prefix/v1/traces");
        let query =
            settings(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://host/prefix?tag=x")]).unwrap();
        assert_eq!(
            query.endpoint.as_str(),
            "http://host/prefix/v1/traces?tag=x"
        );
        let signal = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://wrong"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://host/exact"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=wrong"),
            (
                "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
                "authorization=Bearer%20secret,x-tag=a%2Cb",
            ),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "20"),
            ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "42"),
            ("GST_OTEL_HTTP_CLIENT", "async"),
        ])
        .unwrap();
        assert_eq!(signal.endpoint.as_str(), "https://host/exact");
        assert_eq!(signal.headers["authorization"], "Bearer secret");
        assert_eq!(signal.headers["x-tag"], "a,b");
        assert_eq!(signal.timeout, Duration::from_millis(42));
        assert!(signal.asynchronous);
        for (key, value) in [
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "bad"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_COMPRESSION", "gzip"),
            ("GST_OTEL_HTTP_CLIENT", "typo"),
        ] {
            assert!(settings(&[(key, value)]).is_err());
        }
    }

    #[test]
    fn both_adapters_bound_requests_to_an_unresponsive_receiver() {
        for mode in ["blocking", "async"] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", server.server_addr());
            let (release, waiting) = std::sync::mpsc::channel();
            let receiver = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                let mut body = Vec::new();
                request.as_reader().read_to_end(&mut body).unwrap();
                assert_eq!(body, b"test-body");
                assert_eq!(request.url(), "/v1/traces");
                let _ = waiting.recv_timeout(Duration::from_secs(2));
                let _ = request.respond(tiny_http::Response::empty(200));
            });
            let client = HttpExporter::with_settings(
                settings(&[
                    ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
                    ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "50"),
                    ("GST_OTEL_HTTP_CLIENT", mode),
                ])
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                client.send(Bytes::from_static(b"test-body")),
                Err(FlushError::ExportFailed)
            );
            release.send(()).unwrap();
            receiver.join().unwrap();
        }
    }
}
