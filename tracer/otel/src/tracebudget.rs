//! A journey's admission limits follow its owned correlation token across threads.
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Reason {
    Age = 1,
    Spans = 2,
}

#[derive(Debug)]
pub(crate) struct Budget {
    started_ns: u64,
    max_age_ns: u64,
    max_spans: u64,
    admitted: AtomicU64,
    exhausted: AtomicU8,
}

// Token equality compares budget identity, never a concurrently changing count.
impl PartialEq for Budget {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Budget {
    pub fn new(started_ns: u64, max_age_ns: u64, max_spans: u64) -> Self {
        Self {
            started_ns,
            max_age_ns,
            max_spans,
            admitted: AtomicU64::new(0),
            exhausted: AtomicU8::new(0),
        }
    }

    fn reason(&self) -> Option<Reason> {
        match self.exhausted.load(Relaxed) {
            1 => Some(Reason::Age),
            2 => Some(Reason::Spans),
            _ => None,
        }
    }

    fn reject(&self, reason: Reason) -> Reason {
        // Exhaustion is sticky, even if another thread later presents an older
        // timestamp. Preserve the first reason for consistent accounting.
        let _ = self
            .exhausted
            .compare_exchange(0, reason as u8, Relaxed, Relaxed);
        self.reason().unwrap()
    }

    pub fn admit(&self, ts: u64) -> Result<(), Reason> {
        if let Some(reason) = self.reason() {
            return Err(reason);
        }
        if self.max_age_ns != 0 && ts.saturating_sub(self.started_ns) >= self.max_age_ns {
            return Err(self.reject(Reason::Age));
        }
        if self.max_spans != 0 {
            let mut count = self.admitted.load(Relaxed);
            loop {
                if count >= self.max_spans {
                    return Err(self.reject(Reason::Spans));
                }
                match self
                    .admitted
                    .compare_exchange_weak(count, count + 1, Relaxed, Relaxed)
                {
                    Ok(_) => break,
                    Err(actual) => count = actual,
                }
            }
        }
        // Expiry can race with a reservation. Already reserved slots remain
        // valid when another candidate only discovers the span-count limit.
        if self.reason() == Some(Reason::Age) {
            Err(Reason::Age)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn concurrent_branches_share_one_exact_count_limit() {
        let budget = Budget::new(0, 0, 65);
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    for _ in 0..100 {
                        if budget.admit(1).is_ok() {
                            admitted.fetch_add(1, Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(admitted.load(Relaxed), 65);
        assert_eq!(budget.admit(0), Err(Reason::Spans));
    }

    #[test]
    fn expiry_is_inclusive_and_cannot_be_revived_by_an_older_timestamp() {
        let budget = Budget::new(100, 50, 0);
        assert_eq!(budget.admit(149), Ok(()));
        assert_eq!(budget.admit(150), Err(Reason::Age));
        assert_eq!(budget.admit(101), Err(Reason::Age));
    }
}
