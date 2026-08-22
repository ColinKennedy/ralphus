//! In-process pub/sub for pushing daemon state-change events to connected
//! SSE clients (RAL-167), replacing `board.html`'s fixed-interval polling as
//! the primary live-update path.
//!
//! Fed by [`crate::store::Store::cartographer_log`] — Cartographer already
//! instruments every notable state transition (task/run/session lifecycle,
//! verify starts/results, Guardian review lifecycle; see the "Logging
//! Policy" section of `AGENTS.md`) — so tapping its single write path gives
//! push coverage for run/task/session/guardian/queue/cartographer changes
//! without a second, parallel set of instrumentation call sites. The queue
//! view is a derived, filtered projection over run/task state (see
//! `Store::queue`), so any event carrying a `run_id` implies "the queue may
//! have changed" too — there is no separate queue-specific event kind.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use crate::cartographer::CartographerRow;

/// How a pushed event should be routed client-side, derived from which
/// entity references the underlying Cartographer row carries (RAL-167).
/// This is a coarser, always-populated cousin of [`CartographerRow::scope`],
/// which today is only set at a handful of call sites and can't be relied on
/// as a universal discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Carries a `guardian_id` — a review changed (branches, chat, checks,
    /// merge/rebase state, ...).
    Guardian,
    /// Carries a `run_id` (no `guardian_id`) — a run/task/session (and, by
    /// extension, the queue view) changed.
    Run,
    /// Neither — still Cartographer-worthy, but not scoped to one run or
    /// guardian (e.g. daemon-wide startup recovery events).
    Other,
}

impl EventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Guardian => "guardian",
            EventKind::Run => "run",
            EventKind::Other => "other",
        }
    }

    fn for_row(row: &CartographerRow) -> Self {
        if row.guardian_id.is_some() {
            EventKind::Guardian
        } else if row.run_id.is_some() {
            EventKind::Run
        } else {
            EventKind::Other
        }
    }
}

/// One pushed event: the Cartographer row that was just persisted, plus the
/// derived `kind` used as the SSE `event:` name.
#[derive(Debug, Clone)]
pub struct BusEvent {
    pub kind: EventKind,
    pub row: CartographerRow,
}

struct Subscriber {
    id: u64,
    tx: SyncSender<BusEvent>,
}

/// Bounded per-subscriber channel capacity. A slow/stalled browser tab drops
/// events past this rather than blocking [`EventBus::publish`] — which runs
/// inline inside every Cartographer write, itself inside the daemon-wide
/// `Store` lock (RAL-167's own risk list: a badly-behaved subscriber must
/// never stall the rest of the daemon). The client's slow reconciliation
/// poll (see `board.html`) covers any gap this causes.
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 256;

/// In-memory broadcast registry for daemon state-change events. One instance
/// lives on [`crate::store::Store`], so every `Arc<Mutex<Store>>` holder
/// already serializes access to it via the same lock that guards every other
/// mutation — no separate synchronization story to reason about.
#[derive(Default)]
pub struct EventBus {
    next_id: AtomicU64,
    subscribers: Mutex<Vec<Subscriber>>,
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new subscriber (one per connected SSE client), returning
    /// its id (pass to [`EventBus::unsubscribe`] when the connection closes)
    /// and the receiving end of its event channel.
    pub fn subscribe(&self) -> (u64, Receiver<BusEvent>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        self.subscribers.lock().unwrap().push(Subscriber { id, tx });
        (id, rx)
    }

    /// Remove a subscriber. Idempotent — safe to call even if it was already
    /// pruned by [`EventBus::publish`] noticing a disconnected channel.
    pub fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().unwrap().retain(|s| s.id != id);
    }

    /// Broadcast `row` to every live subscriber. A subscriber whose channel
    /// is momentarily full just misses this event (see
    /// `SUBSCRIBER_CHANNEL_CAPACITY`'s doc comment); one that has fully
    /// disconnected (its SSE thread exited) is pruned here, so a browser tab
    /// closed without a clean teardown doesn't leak a slot forever.
    pub fn publish(&self, row: CartographerRow) {
        let event = BusEvent {
            kind: EventKind::for_row(&row),
            row,
        };
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|s| match s.tx.try_send(event.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    /// Current subscriber count (tests + `/api/daemon` diagnostics).
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(guardian_id: Option<&str>, run_id: Option<&str>) -> CartographerRow {
        CartographerRow {
            id: 1,
            at_ms: 0,
            level: "info".to_string(),
            source: "test".to_string(),
            message: "hi".to_string(),
            scope: None,
            run_id: run_id.map(str::to_string),
            guardian_id: guardian_id.map(str::to_string),
            session_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn subscribe_then_publish_delivers_the_event() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        bus.publish(row(None, Some("run-1")));
        let event = rx.recv().expect("event delivered");
        assert_eq!(event.kind, EventKind::Run);
        assert_eq!(event.row.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn kind_prefers_guardian_over_run() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        bus.publish(row(Some("guardian-1"), Some("run-1")));
        let event = rx.recv().unwrap();
        assert_eq!(event.kind, EventKind::Guardian);
    }

    #[test]
    fn kind_falls_back_to_other_with_no_entity_refs() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        bus.publish(row(None, None));
        let event = rx.recv().unwrap();
        assert_eq!(event.kind, EventKind::Other);
    }

    #[test]
    fn multiple_subscribers_each_get_their_own_copy() {
        let bus = EventBus::new();
        let (_id_a, rx_a) = bus.subscribe();
        let (_id_b, rx_b) = bus.subscribe();
        bus.publish(row(None, Some("run-1")));
        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_ok());
    }

    #[test]
    fn unsubscribe_stops_further_delivery() {
        let bus = EventBus::new();
        let (id, rx) = bus.subscribe();
        bus.unsubscribe(id);
        assert_eq!(bus.subscriber_count(), 0);
        bus.publish(row(None, Some("run-1")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn publish_prunes_a_dropped_receiver() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        drop(rx);
        assert_eq!(bus.subscriber_count(), 1);
        bus.publish(row(None, Some("run-1")));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn full_channel_drops_the_event_but_keeps_the_subscriber() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        for _ in 0..(SUBSCRIBER_CHANNEL_CAPACITY + 10) {
            bus.publish(row(None, Some("run-1")));
        }
        assert_eq!(bus.subscriber_count(), 1);
        // Drain what made it through -- must not panic/deadlock, and must be
        // at most the channel capacity.
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert!(drained <= SUBSCRIBER_CHANNEL_CAPACITY);
        assert!(drained > 0);
    }

    #[test]
    fn event_kind_as_str_matches_sse_event_names() {
        assert_eq!(EventKind::Guardian.as_str(), "guardian");
        assert_eq!(EventKind::Run.as_str(), "run");
        assert_eq!(EventKind::Other.as_str(), "other");
    }
}
