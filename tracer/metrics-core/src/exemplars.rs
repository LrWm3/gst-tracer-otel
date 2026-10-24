//! One recent exemplar per histogram, shared by both export protocols.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exemplar {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub timestamp_ns: u64,
    pub value: f64,
}

#[derive(Default)]
pub(crate) struct Slots([Mutex<Option<Exemplar>>; 3]);

impl Slots {
    pub fn observe(&self, index: usize, context: Exemplar, value: f64) {
        // Contention drops only the exemplar; never its histogram observation.
        if let Ok(mut slot) = self.0[index].try_lock() {
            *slot = Some(Exemplar { value, ..context });
        }
    }
}

type Key = (bool, Vec<String>);
pub type Snapshot = HashMap<Key, [Option<Exemplar>; 3]>;

static SLOTS: LazyLock<Mutex<HashMap<Key, Weak<Slots>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn for_labels(pull: bool, labels: &[&str]) -> Arc<Slots> {
    let mut registry = SLOTS.lock().unwrap();
    let key = (
        pull,
        labels.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
    );
    if let Some(slots) = registry.get(&key).and_then(Weak::upgrade) {
        return slots;
    }
    let slots = Arc::new(Slots::default());
    registry.insert(key, Arc::downgrade(&slots));
    slots
}

pub(crate) fn remove(pull: bool, labels: &[&str]) {
    SLOTS.lock().unwrap().remove(&(
        pull,
        labels.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
    ));
}

/// Snapshot exemplars keyed by pull/push mode and the collector label order.
/// Weak entries do not retain pad descriptions or extend metric lifetimes.
pub fn snapshot() -> Snapshot {
    let mut result = HashMap::new();
    SLOTS.lock().unwrap().retain(|labels, weak| {
        let Some(slots) = weak.upgrade() else {
            return false;
        };
        result.insert(
            labels.clone(),
            std::array::from_fn(|i| *slots.0[i].lock().unwrap()),
        );
        true
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_share_exemplars_and_retirement_releases_them() {
        let labels = ["example", "src", "sink", "path"];
        let first = for_labels(false, &labels);
        let second = for_labels(false, &labels);
        assert!(Arc::ptr_eq(&first, &second));
        let context = Exemplar {
            trace_id: [1; 16],
            span_id: [2; 8],
            timestamp_ns: 3,
            value: 0.0,
        };
        first.observe(0, context, 0.25);
        let key = (false, labels.map(str::to_owned).to_vec());
        assert_eq!(snapshot()[&key][0].unwrap().value, 0.25);
        let pull = for_labels(true, &labels);
        pull.observe(0, context, 0.5);
        assert!(!Arc::ptr_eq(&first, &pull));
        assert_eq!(snapshot()[&(true, key.1.clone())][0].unwrap().value, 0.5);
        remove(false, &labels);
        assert!(snapshot().contains_key(&(true, key.1.clone())));
        assert!(!snapshot().contains_key(&key));
        assert!(!Arc::ptr_eq(&first, &for_labels(false, &labels)));
    }
}
