//! OTLP protobuf encoding for the tracer's fixed span shape. Static attribute
//! bytes live with their descriptor; all scratch buffers belong to the worker.
//! Field numbers follow opentelemetry-proto's trace/v1 Span schema. Differential
//! tests decode these bytes and compare with the public SDK conversion; update
//! those tests alongside any change to the span shape or dependency version.
use bytes::{BufMut, BytesMut};
use opentelemetry::KeyValue;
use opentelemetry_proto::{
    tonic::{common::v1::InstrumentationScope, resource::v1::Resource as ProtoResource},
    transform::common::tonic::{Attributes, ResourceAttributesWithSchema},
};
use opentelemetry_sdk::Resource;
use prost::Message;

use crate::{
    capture::Record,
    export::{Anchor, Config, FlushError},
};

pub(crate) const MAX_BODY: usize = 64 * 1024 * 1024;

fn varint(out: &mut impl BufMut, mut value: u64) {
    while value >= 128 {
        out.put_u8(value as u8 | 128);
        value >>= 7;
    }
    out.put_u8(value as u8);
}
fn width(value: u64) -> usize {
    ((64 - value.leading_zeros()).max(1) as usize).div_ceil(7)
}
fn bytes(out: &mut impl BufMut, field: u64, value: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, value.len() as u64);
    out.put_slice(value);
}
fn integer(out: &mut impl BufMut, field: u64, value: u64) {
    if value != 0 {
        varint(out, field << 3);
        varint(out, value);
    }
}
fn fixed64(out: &mut impl BufMut, field: u64, value: u64) {
    if value != 0 {
        varint(out, (field << 3) | 1);
        out.put_u64_le(value);
    }
}
fn int_attribute(out: &mut Vec<u8>, key: &str, value: i64) {
    let any_len = 1 + width(value as u64);
    let kv_len = 1 + width(key.len() as u64) + key.len() + 1 + width(any_len as u64) + any_len;
    varint(out, (9 << 3) | 2);
    varint(out, kv_len as u64);
    bytes(out, 1, key.as_bytes());
    varint(out, (2 << 3) | 2);
    varint(out, any_len as u64);
    varint(out, 3 << 3);
    varint(out, value as u64);
}

#[derive(Default)]
pub(crate) struct EncodedAttributes {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}
impl EncodedAttributes {
    pub fn new(values: impl IntoIterator<Item = KeyValue>) -> Self {
        let mut result = Self::default();
        for attribute in Attributes::from(values).0 {
            varint(&mut result.bytes, (9 << 3) | 2);
            varint(&mut result.bytes, attribute.encoded_len() as u64);
            attribute.encode(&mut result.bytes).unwrap();
            result.ends.push(result.bytes.len());
        }
        result
    }
    fn append(&self, out: &mut Vec<u8>, remaining: &mut usize) {
        let count = self.ends.len().min(*remaining);
        if count != 0 {
            out.extend_from_slice(&self.bytes[..self.ends[count - 1]]);
        }
        *remaining -= count;
    }
}

pub(crate) struct Encoder {
    resource: Vec<u8>,
    scope_header: Vec<u8>,
    span: Vec<u8>,
    spans: Vec<u8>,
    max_body: usize,
    pub body: BytesMut,
}
impl Encoder {
    pub fn new(resource: &Resource) -> Self {
        let resource: ResourceAttributesWithSchema = resource.into();
        let mut resource_header = Vec::new();
        bytes(
            &mut resource_header,
            1,
            &ProtoResource {
                attributes: resource.attributes.0,
                ..Default::default()
            }
            .encode_to_vec(),
        );
        if let Some(schema) = resource.schema_url {
            bytes(&mut resource_header, 3, schema.as_bytes());
        }
        let mut scope_header = Vec::new();
        bytes(
            &mut scope_header,
            1,
            &InstrumentationScope {
                name: "mars-otel".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            }
            .encode_to_vec(),
        );
        Self {
            resource: resource_header,
            scope_header,
            span: Vec::new(),
            spans: Vec::new(),
            max_body: MAX_BODY,
            body: BytesMut::new(),
        }
    }
    pub fn encode(
        &mut self,
        records: &[Record],
        anchor: &Anchor,
        config: &Config,
    ) -> Result<(), FlushError> {
        self.spans.clear();
        self.body.clear();
        for record in records {
            self.span.clear();
            let span = &mut self.span;
            bytes(span, 1, &record.token.trace_id.to_bytes());
            bytes(span, 2, &record.token.span_id.to_bytes());
            if let Some(state) = &record.token.trace_state {
                let header = state.header();
                if !header.is_empty() {
                    bytes(span, 3, header.as_bytes());
                }
            }
            if record.parent != opentelemetry::trace::SpanId::INVALID {
                bytes(span, 4, &record.parent.to_bytes());
            }
            bytes(span, 5, record.site.span_name(record.operation).as_bytes());
            integer(span, 6, 1); // INTERNAL
            fixed64(span, 7, anchor.nanos(record.start));
            fixed64(span, 8, anchor.nanos(record.end.max(record.start)));
            let total_attributes = record.site.attributes.len()
                + 8
                + usize::from(record.profiled)
                + usize::from(record.list_len.is_some());
            let mut remaining = config.attribute_limit;
            static OPERATIONS: std::sync::LazyLock<[EncodedAttributes; 3]> =
                std::sync::LazyLock::new(|| {
                    [
                        crate::capture::Operation::Push,
                        crate::capture::Operation::Pull,
                        crate::capture::Operation::PushList,
                    ]
                    .map(|op| EncodedAttributes::new([KeyValue::new("gst.operation", op.name())]))
                });
            if config.value_length_limit >= record.operation.name().len() {
                OPERATIONS[record.operation as usize].append(span, &mut remaining);
            } else {
                EncodedAttributes::new([KeyValue::new(
                    "gst.operation",
                    crate::export::text_value(record.operation.name(), config.value_length_limit)
                        .to_string(),
                )])
                .append(span, &mut remaining);
            }
            record
                .site
                .encoded
                .get_or_init(|| EncodedAttributes::new(record.site.attributes.iter().cloned()))
                .append(span, &mut remaining);
            for (key, value) in [
                ("ts.start", record.start as i64),
                ("ts.end", record.end as i64),
                ("buffer.id", record.buffer_id as i64),
                ("buffer.size", record.buffer_size as i64),
            ] {
                if remaining > 0 {
                    int_attribute(span, key, value);
                    remaining -= 1;
                }
            }
            record
                .thread
                .encoded
                .get_or_init(|| {
                    EncodedAttributes::new([
                        KeyValue::new("thread.id", record.thread.id.clone()),
                        KeyValue::new("thread.name", record.thread.name.clone()),
                    ])
                })
                .append(span, &mut remaining);
            if remaining > 0 {
                int_attribute(span, "gst.flow_return", record.flow_return as i64);
                remaining -= 1;
            }
            if record.profiled && remaining > 0 && config.value_length_limit >= 16 {
                // Fixed-size KeyValue/AnyValue: avoid allocating strings and
                // temporary protobuf messages for every profiled span.
                let mut id = [0; 16];
                for (i, byte) in record.token.span_id.to_bytes().into_iter().enumerate() {
                    id[i * 2] = b"0123456789abcdef"[(byte >> 4) as usize];
                    id[i * 2 + 1] = b"0123456789abcdef"[(byte & 15) as usize];
                }
                span.extend_from_slice(b"\x4a\x2a\x0a\x14pyroscope.profile.id\x12\x12\x0a\x10");
                span.extend_from_slice(&id);
                remaining -= 1;
            }
            if let Some(length) = record.list_len.filter(|_| remaining > 0) {
                int_attribute(span, "buffer.list.length", length as i64);
                remaining -= 1;
            }
            integer(
                span,
                10,
                total_attributes.saturating_sub(config.attribute_limit - remaining) as u64,
            );
            if let Some(links) = &record.links {
                for link in links.sdk() {
                    let link: opentelemetry_proto::tonic::trace::v1::span::Link = link.into();
                    varint(span, (13 << 3) | 2);
                    varint(span, link.encoded_len() as u64);
                    link.encode(span).unwrap();
                }
                integer(span, 14, links.dropped as u64);
            }
            if let Some(message) = record.operation.error(record.flow_return) {
                varint(span, (15 << 3) | 2);
                varint(
                    span,
                    (1 + width(message.len() as u64) + message.len() + 2) as u64,
                );
                bytes(span, 2, message.as_bytes());
                integer(span, 3, 2);
            } else {
                bytes(span, 15, &[]);
            }
            varint(span, (16 << 3) | 5);
            span.put_u32_le(
                record.token.flags.to_u8() as u32
                    | 0x100
                    | if record.parent_is_remote { 0x200 } else { 0 },
            );
            if self
                .spans
                .len()
                .saturating_add(span.len())
                .saturating_add(32)
                > self.max_body
            {
                self.spans.clear();
                return Err(FlushError::ExportFailed);
            }
            bytes(&mut self.spans, 2, span);
        }
        let scope_len = self.scope_header.len() + self.spans.len();
        let resource_len = self.resource.len() + 1 + width(scope_len as u64) + scope_len;
        let request_len = 1 + width(resource_len as u64) + resource_len;
        if request_len > self.max_body {
            self.spans.clear();
            return Err(FlushError::ExportFailed);
        }
        self.body.reserve(request_len);
        varint(&mut self.body, 10);
        varint(&mut self.body, resource_len as u64);
        self.body.put_slice(&self.resource);
        varint(&mut self.body, 18);
        varint(&mut self.body, scope_len as u64);
        self.body.put_slice(&self.scope_header);
        self.body.put_slice(&self.spans);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{span_data, tests::record};
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use std::time::SystemTime;
    #[test]
    fn direct_encoding_matches_sdk_conversion_with_limits_flags_and_errors() {
        let resource = Resource::builder_empty()
            .with_attribute(KeyValue::new("service.name", "test"))
            .build();
        let anchor = Anchor {
            hook_ts: 100,
            wall: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000),
        };
        let scope = opentelemetry::InstrumentationScope::builder("mars-otel")
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let mut encoder = Encoder::new(&resource);
        for limit in [0, 1, 2, 4, 5, 6, 7, 8, 9, 128] {
            for (operation, flow, remote) in [
                (crate::capture::Operation::Push, 0, false),
                (crate::capture::Operation::Push, -5, true),
                (crate::capture::Operation::Pull, 0, false),
                (
                    crate::capture::Operation::Pull,
                    gstreamer::ffi::GST_FLOW_EOS,
                    false,
                ),
                (crate::capture::Operation::Pull, -5, true),
                (crate::capture::Operation::PushList, 0, false),
                (crate::capture::Operation::PushList, -5, true),
            ] {
                for (profiled, value_length_limit) in [
                    (false, 4096),
                    (true, 0),
                    (true, 15),
                    (true, 16),
                    (true, 4096),
                ] {
                    let config = Config {
                        attribute_limit: limit,
                        value_length_limit,
                        ..Config::default()
                    };
                    let mut a = record(1);
                    a.operation = operation;
                    a.flow_return = flow;
                    a.parent_is_remote = remote;
                    a.profiled = profiled;
                    a.list_len = (operation == crate::capture::Operation::PushList).then_some(3);
                    let mut b = record(1);
                    b.operation = operation;
                    b.flow_return = flow;
                    b.parent_is_remote = remote;
                    b.profiled = profiled;
                    b.list_len = a.list_len;
                    encoder.encode(&[a], &anchor, &config).unwrap();
                    let decoded = ExportTraceServiceRequest::decode(encoder.body.as_ref()).unwrap();
                    let expected =
                    opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope(
                        vec![span_data(b, &anchor, &config, &scope)],
                        &(&resource).into(),
                    );
                    assert_eq!(
                    decoded.resource_spans, expected,
                    "limit {limit}, flow {flow}, profiled {profiled}, value length {value_length_limit}"
                );
                    let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
                    let profile_id = span
                        .attributes
                        .iter()
                        .find(|a| a.key == "pyroscope.profile.id");
                    assert_eq!(
                        profile_id.is_some(),
                        profiled && limit >= 10 && value_length_limit >= 16
                    );
                }
            }
        }
    }

    #[test]
    fn encoding_preserves_edge_values_schema_and_cached_descriptions() {
        use opentelemetry::trace::{SpanId, TraceFlags};
        let resource = Resource::builder_empty()
            .with_schema_url(
                [
                    KeyValue::new(
                        "array",
                        opentelemetry::Value::Array(opentelemetry::Array::I64(vec![
                            0,
                            -1,
                            i64::MAX,
                        ])),
                    ),
                    KeyValue::new("unicode", "é🙂"),
                ],
                "https://example.test/schema",
            )
            .build();
        let anchor = Anchor {
            hook_ts: 100,
            wall: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        };
        let scope = opentelemetry::InstrumentationScope::builder("mars-otel")
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let mut records: Vec<_> = (0..4).map(record).collect();
        for (i, r) in records.iter_mut().enumerate() {
            r.start = i as u64;
            r.end = 0; // end precedes start: wall duration clamps, raw attribute survives
            r.parent = SpanId::INVALID;
            r.token.flags = TraceFlags::new(0x81);
            r.token.trace_state = None;
            let mut links = crate::propagation::Links::default();
            for id in 100..112 {
                let mut token = record(id).token;
                token.flags = opentelemetry::trace::TraceFlags::new(if id.is_multiple_of(2) {
                    0
                } else {
                    0x81
                });
                links.insert(&token, &r.token);
            }
            r.links = links.finish();
            r.buffer_id = usize::MAX;
            r.buffer_size = 0;
            r.flow_return = i32::MIN;
            r.site = std::sync::Arc::new(crate::capture::Site {
                span_names: Default::default(),
                metrics: Default::default(),
                metric_rejected_epoch: Default::default(),
                attributes: vec![KeyValue::new("src_pad.name", "é🙂")],
                encoded: Default::default(),
                epoch: 0,
            });
        }
        let config = Config::default();
        let mut encoder = Encoder::new(&resource);
        for _ in 0..2 {
            encoder.encode(&records, &anchor, &config).unwrap();
            let decoded = ExportTraceServiceRequest::decode(encoder.body.as_ref()).unwrap();
            let expected =
                opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope(
                    records
                        .iter()
                        .cloned()
                        .map(|r| span_data(r, &anchor, &config, &scope))
                        .collect(),
                    &(&resource).into(),
                );
            assert_eq!(decoded.resource_spans, expected);
        }
        let normal_size = encoder.body.len();
        encoder.max_body = normal_size - 1;
        assert_eq!(
            encoder.encode(&records, &anchor, &config),
            Err(FlushError::ExportFailed)
        );
        assert!(encoder.body.is_empty());
        encoder.max_body = normal_size;
        encoder.encode(&records, &anchor, &config).unwrap();
        assert_eq!(
            ExportTraceServiceRequest::decode(encoder.body.as_ref())
                .unwrap()
                .resource_spans[0]
                .scope_spans[0]
                .spans
                .len(),
            4
        );
    }

    #[test]
    #[ignore = "manual release benchmark; reports timing without timing assertions"]
    fn encoding_benchmark() {
        use std::{
            hint::black_box,
            time::{Duration, Instant},
        };
        let resource = Resource::builder_empty()
            .with_attribute(KeyValue::new("service.name", "encoding-bench"))
            .build();
        let anchor = Anchor {
            hook_ts: 100,
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
        };
        let scope = opentelemetry::InstrumentationScope::builder("mars-otel")
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let site = std::sync::Arc::new(crate::capture::Site {
            span_names: Default::default(),
            metrics: Default::default(),
            metric_rejected_epoch: Default::default(),
            attributes: [
                "src_pad.name",
                "src_pad.element",
                "src_pad.path",
                "sink_pad.name",
                "sink_pad.element",
                "sink_pad.path",
            ]
            .map(|name| KeyValue::new(name, "pipeline/element/pad"))
            .into(),
            encoded: Default::default(),
            epoch: 0,
        });
        let records: Vec<_> = (0..512)
            .map(|i| {
                let mut r = record(i);
                r.site = site.clone();
                r.token.trace_state = None;
                r
            })
            .collect();
        let config = Config::default();
        let mut encoder = Encoder::new(&resource);
        encoder.encode(&records, &anchor, &config).unwrap();
        for round in 0..5 {
            let mut reference = Duration::ZERO;
            let mut direct = Duration::ZERO;
            for iteration in 0..200 {
                // Record duplication is outside both timers. The reference owns
                // records as the former worker did; the new encoder borrows them.
                let input = records.clone();
                let run_reference = || {
                    let started = Instant::now();
                    let body = ExportTraceServiceRequest {
                        resource_spans: opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope(
                            input.into_iter().map(|r| span_data(r, &anchor, &config, &scope)).collect(), &(&resource).into())
                    }.encode_to_vec();
                    black_box(&body);
                    started.elapsed()
                };
                let mut run_direct = || {
                    let started = Instant::now();
                    encoder
                        .encode(black_box(&records), &anchor, &config)
                        .unwrap();
                    black_box(&encoder.body);
                    started.elapsed()
                };
                if iteration % 2 == 0 {
                    reference += run_reference();
                    direct += run_direct();
                } else {
                    direct += run_direct();
                    reference += run_reference();
                }
            }
            println!(
                "ENCODING round={round} spans=102400 reference_ns={} direct_ns={} bytes={}",
                reference.as_nanos(),
                direct.as_nanos(),
                encoder.body.len()
            );
        }
    }
}
