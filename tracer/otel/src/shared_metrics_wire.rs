//! Both encodings consume one collector snapshot; neither measures pipeline work.
use gst_mars_metrics_core::{exemplars::Exemplar, proto as prom};
use opentelemetry_proto::tonic::{
    common::v1::{any_value, AnyValue, KeyValue},
    metrics::v1::{self as otlp, metric::Data},
};
#[cfg(test)]
use std::collections::HashMap;
use std::fmt::Write;

pub(crate) type Examples = gst_mars_metrics_core::exemplars::Snapshot;

fn exemplar(name: &str, metric: &prom::Metric, examples: &Examples) -> Option<Exemplar> {
    let (pull, index) = match name {
        "gst_element_sync_wall_seconds" => (false, 0),
        "gst_element_pull_wall_seconds" => (true, 0),
        "gst_element_sync_cpu_seconds" => (false, 1),
        "gst_element_pull_cpu_seconds" => (true, 1),
        "gst_element_sync_blocked_seconds" => (false, 2),
        "gst_element_pull_blocked_seconds" => (true, 2),
        _ => return None,
    };
    let key: Option<Vec<_>> = ["element", "src_pad", "sink_pad", "path"]
        .iter()
        .map(|key| {
            metric
                .get_label()
                .iter()
                .find(|l| l.name() == *key)
                .map(|l| l.value().to_owned())
        })
        .collect();
    examples.get(&(pull, key?))?[index]
}

fn attributes(metric: &prom::Metric) -> Vec<KeyValue> {
    metric
        .get_label()
        .iter()
        .map(|l| KeyValue {
            key: l.name().into(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(l.value().into())),
            }),
            ..Default::default()
        })
        .collect()
}

pub(crate) fn otlp(
    families: &[prom::MetricFamily],
    examples: &Examples,
    time: u64,
) -> Vec<otlp::Metric> {
    families
        .iter()
        .filter_map(|family| {
            let number = |metric: &prom::Metric, value| otlp::NumberDataPoint {
                attributes: attributes(metric),
                time_unix_nano: time,
                // The registry may retire and recreate a series. An unknown start
                // time is preferable to falsely claiming a process-long total.
                value: Some(otlp::number_data_point::Value::AsDouble(value)),
                ..Default::default()
            };
            let data = match family.get_field_type() {
                prom::MetricType::COUNTER => Data::Sum(otlp::Sum {
                    data_points: family
                        .get_metric()
                        .iter()
                        .map(|m| number(m, m.get_counter().value()))
                        .collect(),
                    aggregation_temporality: otlp::AggregationTemporality::Cumulative as i32,
                    is_monotonic: true,
                }),
                prom::MetricType::GAUGE => Data::Gauge(otlp::Gauge {
                    data_points: family
                        .get_metric()
                        .iter()
                        .map(|m| number(m, m.get_gauge().value()))
                        .collect(),
                }),
                prom::MetricType::HISTOGRAM => Data::Histogram(otlp::Histogram {
                    aggregation_temporality: otlp::AggregationTemporality::Cumulative as i32,
                    data_points: family
                        .get_metric()
                        .iter()
                        .map(|m| {
                            let h = m.get_histogram();
                            let mut previous = 0;
                            let mut counts = Vec::new();
                            let mut bounds = Vec::new();
                            for b in h.get_bucket() {
                                if !b.upper_bound().is_finite() {
                                    continue;
                                }
                                counts.push(b.cumulative_count().saturating_sub(previous));
                                previous = b.cumulative_count();
                                bounds.push(b.upper_bound());
                            }
                            counts.push(h.sample_count().saturating_sub(previous));
                            otlp::HistogramDataPoint {
                                attributes: attributes(m),
                                time_unix_nano: time,
                                count: h.sample_count(),
                                sum: Some(h.sample_sum()),
                                bucket_counts: counts,
                                explicit_bounds: bounds,
                                exemplars: exemplar(family.name(), m, examples)
                                    .into_iter()
                                    .map(|e| otlp::Exemplar {
                                        time_unix_nano: e.timestamp_ns,
                                        span_id: e.span_id.to_vec(),
                                        trace_id: e.trace_id.to_vec(),
                                        value: Some(otlp::exemplar::Value::AsDouble(e.value)),
                                        ..Default::default()
                                    })
                                    .collect(),
                                ..Default::default()
                            }
                        })
                        .collect(),
                }),
                _ => return None,
            };
            Some(otlp::Metric {
                name: family.name().into(),
                description: family.help().into(),
                data: Some(data),
                ..Default::default()
            })
        })
        .collect()
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}
fn labels(metric: &prom::Metric, bound: Option<f64>) -> String {
    let mut labels: Vec<_> = metric
        .get_label()
        .iter()
        .map(|l| format!("{}=\"{}\"", l.name(), escape(l.value())))
        .collect();
    if let Some(bound) = bound {
        labels.push(format!(
            "le=\"{}\"",
            if bound.is_infinite() {
                "+Inf".into()
            } else {
                bound.to_string()
            }
        ));
    }
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", labels.join(","))
    }
}

/// OpenMetrics 1.0, including one recent, actually observed exemplar per histogram.
/// Counter sample names receive the standard `_total` suffix, also used by OTLP
/// to Prometheus translation. Existing standalone text exposition is unchanged.
pub(crate) fn openmetrics(families: &[prom::MetricFamily], examples: &Examples) -> String {
    let mut text = String::new();
    for family in families {
        let name = family.name();
        let kind = family.get_field_type();
        let (base, type_name) = match kind {
            prom::MetricType::COUNTER => (name.strip_suffix("_total").unwrap_or(name), "counter"),
            prom::MetricType::GAUGE => (name, "gauge"),
            prom::MetricType::HISTOGRAM => (name, "histogram"),
            _ => continue,
        };
        writeln!(
            text,
            "# HELP {base} {}\n# TYPE {base} {type_name}",
            family.help().replace('\\', "\\\\").replace('\n', "\\n")
        )
        .unwrap();
        for metric in family.get_metric() {
            let labels = labels(metric, None);
            match kind {
                prom::MetricType::COUNTER => {
                    writeln!(
                        text,
                        "{base}_total{labels} {}",
                        metric.get_counter().value()
                    )
                    .unwrap();
                }
                prom::MetricType::GAUGE => {
                    writeln!(text, "{name}{labels} {}", metric.get_gauge().value()).unwrap();
                }
                prom::MetricType::HISTOGRAM => {
                    let h = metric.get_histogram();
                    let mut example = exemplar(name, metric, examples);
                    let buckets = h
                        .get_bucket()
                        .iter()
                        .filter(|b| b.upper_bound().is_finite())
                        .map(|b| (b.upper_bound(), b.cumulative_count()))
                        .chain(std::iter::once((f64::INFINITY, h.sample_count())));
                    for (bound, count) in buckets {
                        write!(
                            text,
                            "{name}_bucket{} {count}",
                            self::labels(metric, Some(bound))
                        )
                        .unwrap();
                        if let Some(e) = example.filter(|e| e.value <= bound) {
                            write!(
                                text,
                                " # {{trace_id=\"{}\",span_id=\"{}\"}} {} {}.{:09}",
                                opentelemetry::trace::TraceId::from_bytes(e.trace_id),
                                opentelemetry::trace::SpanId::from_bytes(e.span_id),
                                e.value,
                                e.timestamp_ns / 1_000_000_000,
                                e.timestamp_ns % 1_000_000_000
                            )
                            .unwrap();
                            example = None;
                        }
                        text.push('\n');
                    }
                    writeln!(
                        text,
                        "{name}_sum{labels} {}\n{name}_count{labels} {}",
                        h.sample_sum(),
                        h.sample_count()
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
        }
    }
    text.push_str("# EOF\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn histogram_buckets_and_exemplar_agree_across_protocols() {
        let mut family = prom::MetricFamily::default();
        family.set_name("gst_element_sync_wall_seconds".into());
        family.set_field_type(prom::MetricType::HISTOGRAM);
        let mut metric = prom::Metric::default();
        for key in ["element", "src_pad", "sink_pad", "path"] {
            let mut l = prom::LabelPair::default();
            l.set_name(key.into());
            l.set_value("a\"b\nc\\d".into());
            metric.label.push(l);
        }
        let mut h = prom::Histogram::default();
        h.set_sample_count(7);
        h.set_sample_sum(3.5);
        for (bound, count) in [(0.1, 2), (0.5, 5)] {
            let mut b = prom::Bucket::default();
            b.set_upper_bound(bound);
            b.set_cumulative_count(count);
            h.bucket.push(b);
        }
        metric.set_histogram(h);
        family.metric.push(metric);
        let example = Exemplar {
            trace_id: [1; 16],
            span_id: [2; 8],
            timestamp_ns: 1_234_567_890,
            value: 0.25,
        };
        let examples = HashMap::from([(
            (false, vec!["a\"b\nc\\d".into(); 4]),
            [Some(example), None, None],
        )]);
        let families = vec![family];
        let converted = otlp(&families, &examples, 2_000_000_000);
        let Some(Data::Histogram(hist)) = &converted[0].data else {
            panic!()
        };
        assert_eq!(hist.data_points[0].bucket_counts, vec![2, 3, 2]);
        assert_eq!(hist.data_points[0].count, 7);
        assert_eq!(hist.data_points[0].exemplars[0].span_id, vec![2; 8]);
        let text = openmetrics(&families, &examples);
        assert!(text.contains("le=\"0.5\"} 5 # {trace_id="));
        assert!(text.contains("0.25 1.234567890"));
        assert!(text.contains("a\\\"b\\nc\\\\d"));
        assert_eq!(text.matches(" # {").count(), 1);
        assert!(text.ends_with("# EOF\n"));
    }
}
