//! Retire series when their objects disappear or move. All registry work runs
//! during cache construction, scraping or maintenance, never on a cache hit.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, OnceLock},
    time::Duration,
};

use gstreamer::{self as gst, prelude::*};

type Key = (&'static str, Vec<String>);
type Owners = Vec<Owner>;

struct Owner {
    object: glib::WeakRef<gst::Object>,
    pad: Option<(String, usize)>,
    path: String,
}

impl Owner {
    fn new(object: &gst::Object) -> Self {
        // Track pads through their owning element. Maintenance looks up the
        // live pad by name before comparing its address; a stored address is
        // only an identity token and is never dereferenced.
        let parent = object
            .downcast_ref::<gst::Pad>()
            .and_then(|pad| pad.parent_element());
        let (owner, pad) = match parent.as_ref() {
            Some(parent) => (
                parent.upcast_ref(),
                Some((object.name().to_string(), object.as_ptr() as usize)),
            ),
            None => (object, None),
        };
        Self {
            object: owner.downgrade(),
            pad,
            path: object.path_string().to_string(),
        }
    }

    fn is_live(&self) -> bool {
        let Some(object) = self.object.upgrade() else {
            return false;
        };
        if let Some((name, identity)) = &self.pad {
            object
                .downcast_ref::<gst::Element>()
                .and_then(|element| element.static_pad(name))
                .is_some_and(|pad| {
                    pad.as_ptr() as usize == *identity && pad.path_string().as_str() == self.path
                })
        } else {
            object.path_string().as_str() == self.path
        }
    }

    fn same(&self, other: &Self) -> bool {
        self.path == other.path
            && self.pad == other.pad
            && self.object.upgrade() == other.object.upgrade()
    }
}

struct Series {
    owners: Vec<Owners>,
    remove: fn(&[&str]),
}

static SERIES: LazyLock<Mutex<HashMap<Key, Series>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static MAINTENANCE: OnceLock<()> = OnceLock::new();

pub fn with_series<T>(
    family: &'static str,
    labels: &[&str],
    objects: &[&gst::Object],
    remove: fn(&[&str]),
    create: impl FnOnce() -> T,
) -> T {
    MAINTENANCE.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            prune();
        });
    });
    let owners: Owners = objects.iter().map(|object| Owner::new(object)).collect();
    let key = (
        family,
        labels.iter().map(|label| (*label).to_owned()).collect(),
    );
    let mut registry = SERIES.lock().unwrap();
    let series = registry.entry(key).or_insert_with(|| Series {
        owners: Vec::new(),
        remove,
    });
    if !series.owners.is_empty() {
        series
            .owners
            .retain(|owners| owners.iter().all(Owner::is_live));
        if series.owners.is_empty() {
            // A replacement graph can reuse labels before the next scrape or
            // maintenance tick. Start its counters afresh in that case too.
            (series.remove)(labels);
        }
    }
    // Multiple cache generations, or multiple live owners with identical labels,
    // share one series. Removing one owner must not remove another's metrics.
    if !series.owners.iter().any(|existing| {
        existing.len() == owners.len() && existing.iter().zip(&owners).all(|(a, b)| a.same(b))
    }) {
        series.owners.push(owners);
    }
    // Serialize handle creation with removal so a cache never gets an orphaned
    // handle because maintenance removed the series between these operations.
    create()
}

pub fn prune() {
    SERIES.lock().unwrap().retain(|(_, labels), series| {
        series
            .owners
            .retain(|owners| owners.iter().all(Owner::is_live));
        if series.owners.is_empty() {
            let labels: Vec<_> = labels.iter().map(String::as_str).collect();
            (series.remove)(&labels);
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::IntCounterVec;

    static COUNTERS: LazyLock<IntCounterVec> = LazyLock::new(|| {
        IntCounterVec::new(prometheus::Opts::new("lifecycle_test", "test"), &["label"]).unwrap()
    });

    #[test]
    fn shared_labels_survive_one_owner_and_reset_after_the_last_owner_dies() {
        gst::init().unwrap();
        let first = gst::Bin::with_name("same_name");
        let second = gst::Bin::with_name("same_name");
        let acquire = |object: &gst::Object| {
            with_series(
                "lifecycle_test",
                &["shared"],
                &[object],
                |labels| {
                    COUNTERS.remove_label_values(labels).unwrap();
                },
                || COUNTERS.with_label_values(&["shared"]),
            )
        };
        acquire(first.upcast_ref()).inc();
        acquire(second.upcast_ref()).inc();
        drop(first);
        prune();
        assert_eq!(acquire(second.upcast_ref()).get(), 2);
        drop(second);
        let third = gst::Bin::with_name("same_name");
        // Reuse before a maintenance tick must not inherit the old counts.
        assert_eq!(acquire(third.upcast_ref()).get(), 0);
        acquire(third.upcast_ref()).inc();
        drop(third);
        prune();
        let fourth = gst::Bin::with_name("same_name");
        assert_eq!(acquire(fourth.upcast_ref()).get(), 0);
    }
}
