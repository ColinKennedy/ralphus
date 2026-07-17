//! RAL-121: background, priority-ordered computation of each guardian's
//! preliminary (git-log-only) change summary.
//!
//! The review page only ever displays one guardian at a time, so whichever
//! guardian is currently open in the UI is computed at [`Priority::High`];
//! every other guardian that needs a summary recompute (a newly-ready branch
//! noticed by the scheduler, or one merely visible while scrolling the review
//! list) is computed at [`Priority::Low`] on the same background workers,
//! never blocking a request thread or the store lock on git subprocess calls.
//! Opening a different review [`SummaryQueue::promote`]s its pending job to
//! High rather than running a second, synchronous computation.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Low,
    High,
}

struct Inner {
    high: VecDeque<String>,
    low: VecDeque<String>,
    /// Guardian ids currently sitting in `high` or `low` — lets `enqueue`/
    /// `promote` be idempotent instead of piling up duplicate entries for a
    /// guardian that's already waiting to be recomputed.
    queued: HashSet<String>,
    shutdown: bool,
}

/// A priority queue of guardian ids awaiting [`recompute_preliminary_summary`].
/// Cheap to construct; holds no store reference itself so tests can enqueue
/// against it without any workers running.
pub struct SummaryQueue {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl SummaryQueue {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                high: VecDeque::new(),
                low: VecDeque::new(),
                queued: HashSet::new(),
                shutdown: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// Queue a guardian for recompute at `priority`. A no-op if it's already
    /// queued at `High`; upgrades an already-queued `Low` entry to `High`
    /// rather than adding a second entry. A guardian currently being
    /// processed (already popped) is not tracked as queued, so this simply
    /// re-queues it — at worst a harmless extra recompute once the in-flight
    /// one finishes, which correctly picks up any branch state that changed
    /// meanwhile.
    pub fn enqueue(&self, id: &str, priority: Priority) {
        let mut inner = self.inner.lock().expect("summary queue mutex poisoned");
        if inner.queued.contains(id) {
            if priority == Priority::High {
                if let Some(pos) = inner.low.iter().position(|x| x == id) {
                    inner.low.remove(pos);
                    inner.high.push_back(id.to_string());
                    self.cv.notify_all();
                }
            }
            return;
        }
        inner.queued.insert(id.to_string());
        match priority {
            Priority::High => inner.high.push_back(id.to_string()),
            Priority::Low => inner.low.push_back(id.to_string()),
        }
        self.cv.notify_all();
    }

    /// Promote a guardian to the front of the High queue — the "the user just
    /// opened this review" signal. If it's queued at Low, moves it to the
    /// front of High. If it's queued at High, moves it to the front (so it's
    /// picked up next rather than waiting behind other High entries). If it
    /// isn't queued at all (never enqueued, or already finished), wakes it up
    /// by enqueueing fresh at High — never runs synchronously on the caller's
    /// thread.
    pub fn promote(&self, id: &str) {
        let mut inner = self.inner.lock().expect("summary queue mutex poisoned");
        if let Some(pos) = inner.low.iter().position(|x| x == id) {
            inner.low.remove(pos);
            inner.high.push_front(id.to_string());
            self.cv.notify_all();
            return;
        }
        if let Some(pos) = inner.high.iter().position(|x| x == id) {
            inner.high.remove(pos);
            inner.high.push_front(id.to_string());
            self.cv.notify_all();
            return;
        }
        inner.queued.insert(id.to_string());
        inner.high.push_front(id.to_string());
        self.cv.notify_all();
    }

    /// Block until an id is available (High before Low, FIFO within a tier),
    /// or `None` once [`Self::shutdown`] has been called and the queue is
    /// drained.
    fn pop(&self) -> Option<String> {
        let mut inner = self.inner.lock().expect("summary queue mutex poisoned");
        loop {
            if let Some(id) = inner.high.pop_front().or_else(|| inner.low.pop_front()) {
                inner.queued.remove(&id);
                return Some(id);
            }
            if inner.shutdown {
                return None;
            }
            inner = self.cv.wait(inner).expect("summary queue mutex poisoned");
        }
    }

    /// Stop the queue once everything already queued has been popped — lets
    /// [`worker_loop`] return instead of blocking forever. Existing entries
    /// still drain first; this does not discard queued work.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().expect("summary queue mutex poisoned");
        inner.shutdown = true;
        self.cv.notify_all();
    }
}

/// Pop guardian ids forever (until `shutdown` and drained) and recompute each
/// one's preliminary summary. Safe to run on multiple threads concurrently —
/// [`SummaryQueue::pop`] hands out each id to exactly one worker.
pub fn worker_loop(queue: &SummaryQueue, store: &Arc<Mutex<Store>>) {
    while let Some(id) = queue.pop() {
        crate::guardian_merge::recompute_preliminary_summary(store, &id);
    }
}

/// Spawn `count` (min 1) persistent worker threads draining `queue`.
pub fn spawn_workers(
    queue: &Arc<SummaryQueue>,
    store: &Arc<Mutex<Store>>,
    count: usize,
) -> Vec<std::thread::JoinHandle<()>> {
    (0..count.max(1))
        .map(|_| {
            let queue = Arc::clone(queue);
            let store = Arc::clone(store);
            std::thread::spawn(move || worker_loop(&queue, &store))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_dedups_same_id() {
        let q = SummaryQueue::new();
        q.enqueue("a", Priority::Low);
        q.enqueue("a", Priority::Low);
        assert_eq!(q.pop().as_deref(), Some("a"));
        q.shutdown();
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn high_priority_pops_before_low() {
        let q = SummaryQueue::new();
        q.enqueue("low-1", Priority::Low);
        q.enqueue("high-1", Priority::High);
        assert_eq!(q.pop().as_deref(), Some("high-1"));
        assert_eq!(q.pop().as_deref(), Some("low-1"));
    }

    #[test]
    fn enqueue_high_upgrades_queued_low_entry() {
        let q = SummaryQueue::new();
        q.enqueue("a", Priority::Low);
        q.enqueue("b", Priority::High);
        q.enqueue("a", Priority::High);
        // "a" was upgraded to High, so it now pops before "b" was even though
        // "b" was enqueued at High first -- "a" moved to the back of High.
        assert_eq!(q.pop().as_deref(), Some("b"));
        assert_eq!(q.pop().as_deref(), Some("a"));
    }

    #[test]
    fn promote_moves_queued_low_entry_to_front_of_high() {
        let q = SummaryQueue::new();
        q.enqueue("other", Priority::High);
        q.enqueue("target", Priority::Low);
        q.promote("target");
        assert_eq!(q.pop().as_deref(), Some("target"));
        assert_eq!(q.pop().as_deref(), Some("other"));
    }

    #[test]
    fn promote_wakes_unqueued_id_at_high_priority() {
        let q = SummaryQueue::new();
        q.promote("cold");
        assert_eq!(q.pop().as_deref(), Some("cold"));
    }

    #[test]
    fn shutdown_drains_existing_entries_before_stopping() {
        let q = SummaryQueue::new();
        q.enqueue("a", Priority::Low);
        q.enqueue("b", Priority::Low);
        q.shutdown();
        assert_eq!(q.pop().as_deref(), Some("a"));
        assert_eq!(q.pop().as_deref(), Some("b"));
        assert_eq!(q.pop(), None);
    }
}
