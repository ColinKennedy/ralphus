//! Shared builds of the board's heaviest read responses (STRESS-003,
//! `perf/stress/followups/`).
//!
//! `GET /api/tasks`, `/api/task-index` and `/api/queue` each rebuilt every
//! squad's view on every request. Concurrent requests -- every open tab
//! refreshes on every pushed event -- each paid the whole cost, which capped
//! reads at ~20 req/s and let a burst of board loads saturate the daemon.
//! Here, one build of a response is shared by every request for the same
//! endpoint and query that arrives while it is current, and requests that
//! arrive while it is being built wait for it rather than starting their own.
//!
//! A cached response is current while the store's [data generation] is
//! unchanged, and for at most `max_age`. The generation moves on every write
//! the writer connection commits, so a response never outlives the data it
//! was built from; `max_age` bounds the few in-memory values a response also
//! carries (the running count, the downtime flag).
//!
//! [data generation]: data_generation

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

static DATA_GENERATION: AtomicU64 = AtomicU64::new(0);
static COMMIT_PENDING: AtomicBool = AtomicBool::new(false);

/// A number that changes whenever the store's data may have changed.
pub(crate) fn data_generation() -> u64 {
    DATA_GENERATION.load(Ordering::SeqCst)
}

/// Install the commit hook that moves the data generation. Called on the
/// writer connection only (pooled connections are read-only).
///
/// SQLite runs a commit hook *before* the commit becomes visible, so a
/// response built in that instant could still read the old data under the
/// new generation. The hook therefore also marks a commit as pending, and
/// [`note_writer_released`] -- called as the store lock is released, after
/// the commit is visible -- moves the generation once more.
pub(crate) fn install_commit_hook(conn: &rusqlite::Connection) {
    conn.commit_hook(Some(|| {
        DATA_GENERATION.fetch_add(1, Ordering::SeqCst);
        COMMIT_PENDING.store(true, Ordering::SeqCst);
        false
    }));
}

/// See [`install_commit_hook`]. Called by `StoreGuard`'s drop.
pub(crate) fn note_writer_released() {
    if COMMIT_PENDING.swap(false, Ordering::SeqCst) {
        DATA_GENERATION.fetch_add(1, Ordering::SeqCst);
    }
}

/// One cached response.
pub(crate) struct Cached {
    pub(crate) status: u16,
    pub(crate) body: String,
}

enum Slot {
    /// A build for this generation is in progress.
    Building { generation: u64 },
    Ready {
        generation: u64,
        built_at: Instant,
        value: Arc<Cached>,
    },
}

/// Responses keyed by endpoint and query.
pub(crate) struct ResponseCache {
    slots: Mutex<HashMap<String, Slot>>,
    built: Condvar,
    /// Where the data generation comes from: [`data_generation`], except in
    /// tests that need a generation no other test can move.
    generation: fn() -> u64,
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::with_generation(data_generation)
    }
}

/// Removes a `Building` slot and wakes the waiters if a build ends without
/// storing a result (an error, a stale build, or a panic).
struct BuildingGuard<'a> {
    cache: &'a ResponseCache,
    key: &'a str,
    armed: bool,
}

impl Drop for BuildingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut slots = self
                .cache
                .slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if matches!(slots.get(self.key), Some(Slot::Building { .. })) {
                slots.remove(self.key);
            }
            self.cache.built.notify_all();
        }
    }
}

/// How long a request waits on another request's build of the same response
/// before building its own.
const WAIT_FOR_BUILD: Duration = Duration::from_secs(30);

impl ResponseCache {
    fn with_generation(generation: fn() -> u64) -> Self {
        Self {
            slots: Mutex::default(),
            built: Condvar::new(),
            generation,
        }
    }

    /// The current response for `key`, building it with `build` if there is
    /// none. Only a `200` is kept; anything else is returned to this caller
    /// alone and the next request builds again.
    pub(crate) fn get_or_build(
        &self,
        key: &str,
        max_age: Duration,
        build: impl FnOnce() -> Cached,
    ) -> Arc<Cached> {
        let generation = (self.generation)();
        {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            let wait_started = Instant::now();
            loop {
                match slots.get(key) {
                    Some(Slot::Ready {
                        generation: g,
                        built_at,
                        value,
                    }) if *g == generation && built_at.elapsed() < max_age => {
                        return Arc::clone(value);
                    }
                    Some(Slot::Building { generation: g })
                        if *g == generation && wait_started.elapsed() < WAIT_FOR_BUILD =>
                    {
                        let (next, _) = self
                            .built
                            .wait_timeout(slots, WAIT_FOR_BUILD)
                            .unwrap_or_else(PoisonError::into_inner);
                        slots = next;
                    }
                    _ => {
                        slots.insert(key.to_string(), Slot::Building { generation });
                        break;
                    }
                }
            }
        }
        let mut guard = BuildingGuard {
            cache: self,
            key,
            armed: true,
        };
        let built_at = Instant::now();
        let value = Arc::new(build());
        // A write that landed during the build may or may not be in it:
        // serve it to this caller, but do not keep it.
        if value.status == 200 && (self.generation)() == generation {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            slots.insert(
                key.to_string(),
                Slot::Ready {
                    generation,
                    built_at,
                    value: Arc::clone(&value),
                },
            );
            guard.armed = false;
            self.built.notify_all();
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn ok(body: &str) -> Cached {
        Cached {
            status: 200,
            body: body.to_string(),
        }
    }

    static REUSE_GENERATION: AtomicU64 = AtomicU64::new(0);
    fn reuse_generation() -> u64 {
        REUSE_GENERATION.load(Ordering::SeqCst)
    }

    #[test]
    fn a_current_response_is_reused_and_a_new_generation_invalidates_it() {
        let cache = ResponseCache::with_generation(reuse_generation);
        let builds = AtomicUsize::new(0);
        let build = || {
            builds.fetch_add(1, Ordering::SeqCst);
            ok("v")
        };
        cache.get_or_build("k", Duration::from_secs(60), build);
        cache.get_or_build("k", Duration::from_secs(60), build);
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        REUSE_GENERATION.fetch_add(1, Ordering::SeqCst);
        cache.get_or_build("k", Duration::from_secs(60), build);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// The writer's commit hook and the store lock's release both move the
    /// real generation (see `install_commit_hook`).
    #[test]
    fn a_commit_on_the_writer_moves_the_data_generation() {
        let store = crate::store::Store::open_in_memory().expect("store");
        let before = data_generation();
        store
            .conn
            .execute_batch("CREATE TABLE gen_probe (x INTEGER); INSERT INTO gen_probe VALUES (1);")
            .expect("write");
        assert!(data_generation() > before);
        COMMIT_PENDING.store(true, Ordering::SeqCst);
        let mid = data_generation();
        note_writer_released();
        assert!(data_generation() > mid);
    }

    static FIXED_GENERATION: AtomicU64 = AtomicU64::new(0);
    fn fixed_generation() -> u64 {
        FIXED_GENERATION.load(Ordering::SeqCst)
    }

    #[test]
    fn max_age_bounds_reuse() {
        let cache = ResponseCache::with_generation(fixed_generation);
        let builds = AtomicUsize::new(0);
        let build = || {
            builds.fetch_add(1, Ordering::SeqCst);
            ok("v")
        };
        cache.get_or_build("k", Duration::ZERO, build);
        cache.get_or_build("k", Duration::ZERO, build);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn errors_are_not_kept_and_keys_are_separate() {
        let cache = ResponseCache::with_generation(fixed_generation);
        let builds = AtomicUsize::new(0);
        let failing = || {
            builds.fetch_add(1, Ordering::SeqCst);
            Cached {
                status: 500,
                body: String::new(),
            }
        };
        cache.get_or_build("a", Duration::from_secs(60), failing);
        cache.get_or_build("a", Duration::from_secs(60), failing);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        let other = cache.get_or_build("b", Duration::from_secs(60), || ok("b"));
        assert_eq!(other.body, "b");
    }

    /// Concurrent requests share one build instead of each building.
    #[test]
    fn concurrent_requests_share_one_build() {
        let cache = Arc::new(ResponseCache::with_generation(fixed_generation));
        let builds = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let (cache, builds) = (Arc::clone(&cache), Arc::clone(&builds));
            handles.push(std::thread::spawn(move || {
                cache
                    .get_or_build("herd", Duration::from_secs(60), || {
                        builds.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(200));
                        ok("shared")
                    })
                    .body
                    .clone()
            }));
        }
        for h in handles {
            assert_eq!(h.join().unwrap(), "shared");
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    /// A build that panics releases its waiters instead of leaving them
    /// stuck on a slot nobody will fill.
    #[test]
    fn a_panicking_build_does_not_strand_waiters() {
        let cache = ResponseCache::with_generation(fixed_generation);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_build("p", Duration::from_secs(60), || panic!("boom"));
        }));
        assert!(result.is_err());
        let value = cache.get_or_build("p", Duration::from_secs(60), || ok("after"));
        assert_eq!(value.body, "after");
    }
}
