//! Two-level pprof: exclusive GStreamer owner -> CUDA kernel. Profile timestamps
//! describe submission windows; weights describe measured device durations.
use super::{Counters, Key, Value};
use prost::Message;
use pyroscope::{
    backend::{ReportBatch, ReportData},
    encode::gen::google::{Function, Label, Line, Location, Profile, Sample, ValueType},
};
use std::{collections::HashMap, sync::atomic::Ordering::Relaxed};

pub(super) fn batches(groups: HashMap<Key, Value>, counters: &Counters) -> Vec<(ReportBatch, u64)> {
    let mut buckets: HashMap<u64, Builder> = HashMap::new();
    for (key, value) in groups {
        buckets
            .entry(key.bucket)
            .or_insert_with(|| Builder::new(key.bucket))
            .sample(key, value);
    }
    buckets
        .into_values()
        .filter_map(|builder| {
            if builder.profile.encoded_len() > 8 * 1024 * 1024 {
                counters
                    .dropped_processing
                    .fetch_add(builder.kernels, Relaxed);
                None
            } else {
                Some((
                    ReportBatch {
                        profile_type: "gpu_kernel".into(),
                        data: ReportData::RawPprof(builder.profile.encode_to_vec()),
                    },
                    builder.kernels,
                ))
            }
        })
        .collect()
}
struct Builder {
    profile: Profile,
    strings: HashMap<String, i64>,
    frames: HashMap<String, u64>,
    kernels: u64,
}
impl Builder {
    fn new(bucket: u64) -> Self {
        let mut result = Self {
            profile: Profile {
                time_nanos: bucket.saturating_mul(1_000_000_000).min(i64::MAX as u64) as i64,
                duration_nanos: 1_000_000_000,
                // Each weight is already a measured nanosecond, not a sampling tick.
                period: 1,
                ..Default::default()
            },
            strings: HashMap::new(),
            frames: HashMap::new(),
            kernels: 0,
        };
        result.string("");
        let kind = ValueType {
            r#type: result.string("gpu"),
            unit: result.string("nanoseconds"),
        };
        result.profile.sample_type.push(kind);
        result.profile.period_type = Some(kind);
        result
    }
    fn string(&mut self, value: &str) -> i64 {
        if let Some(id) = self.strings.get(value) {
            return *id;
        }
        let id = self.profile.string_table.len() as i64;
        self.profile.string_table.push(value.into());
        self.strings.insert(value.into(), id);
        id
    }
    fn frame(&mut self, name: &str) -> u64 {
        if let Some(id) = self.frames.get(name) {
            return *id;
        }
        let name_id = self.string(name);
        let id = self.profile.function.len() as u64 + 1;
        self.profile.function.push(Function {
            id,
            name: name_id,
            ..Default::default()
        });
        self.profile.location.push(Location {
            id,
            line: vec![Line {
                function_id: id,
                ..Default::default()
            }],
            ..Default::default()
        });
        self.frames.insert(name.into(), id);
        id
    }
    fn label(&mut self, key: &str, value: &str) -> Label {
        Label {
            key: self.string(key),
            str: self.string(value),
            ..Default::default()
        }
    }
    fn sample(&mut self, key: Key, value: Value) {
        let location_id = vec![self.frame(&key.kernel), self.frame(&key.element)];
        let mut label = vec![self.label("gpu_device", &key.device.to_string())];
        if key.ids != [0; 3] {
            label.push(self.label(
                "trace_id",
                &format!("{:016x}{:016x}", key.ids[0], key.ids[1]),
            ));
            label.push(self.label("span_id", &format!("{:016x}", key.ids[2])));
        }
        self.profile.sample.push(Sample {
            location_id,
            value: vec![value.nanos.min(i64::MAX as u64) as i64],
            label,
        });
        self.kernels += value.kernels;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_kernel_different_owners_and_spans_are_separate_two_frame_samples() {
        let mut groups = HashMap::new();
        for span in [3, 4] {
            groups.insert(
                Key {
                    bucket: 42,
                    ids: [1, 2, span],
                    element: format!("[element] stage{span}").into(),
                    kernel: "resize_kernel".into(),
                    device: 0,
                },
                Value {
                    nanos: span * 1_000_000,
                    kernels: 2,
                },
            );
        }
        let batches = batches(groups, &Counters::default());
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].1, 4);
        assert_eq!(batches[0].0.profile_type, "gpu_kernel");
        let ReportData::RawPprof(ref bytes) = batches[0].0.data else {
            panic!()
        };
        let p = Profile::decode(bytes.as_slice()).unwrap();
        assert_eq!(p.time_nanos, 42_000_000_000);
        assert_eq!(p.sample.len(), 2);
        let mut spans = Vec::new();
        for sample in &p.sample {
            assert_eq!(sample.location_id.len(), 2);
            let names: Vec<_> = sample
                .location_id
                .iter()
                .map(|id| {
                    let f = p.location[(*id - 1) as usize].line[0].function_id;
                    p.string_table[p.function[(f - 1) as usize].name as usize].as_str()
                })
                .collect();
            assert_eq!(names[0], "resize_kernel");
            assert!(names[1].starts_with("[element] stage"));
            let span = sample
                .label
                .iter()
                .find(|l| p.string_table[l.key as usize] == "span_id")
                .unwrap();
            spans.push(p.string_table[span.str as usize].clone());
        }
        spans.sort();
        assert_eq!(spans, ["0000000000000003", "0000000000000004"]);
        assert_eq!(p.sample.iter().map(|s| s.value[0]).sum::<i64>(), 7_000_000);
    }
}
