//! The host-owned session floor: a one-way tier ratchet per session.
//!
//! Why this lives in the filter and not in Switchyard: Capability mode's
//! classifier is per-request/stateless, and Switchyard's `session_affinity`
//! is a *first-decision-wins* latch — it freezes turn 1's tier in either
//! direction, so it can pin a session to `weak` and block a needed upgrade.
//! Switchyard's state is also in-process, TTL-bound, and not seedable from
//! outside. The no-downgrade guarantee ("once a session reaches `strong` it
//! never silently drops back to `weak`") therefore has to be owned host-side:
//! the filter clamps every decision to `max(floor, decision)` and ratchets
//! the floor upward only.
//!
//! Honesty note: this cache has the same loss profile as Switchyard's own
//! state — a restart, failover, or replica hop wipes it. What protects those
//! cases is the filter's don't-overwrite-on-failure rule (pass through
//! unmodified), which degrades to "route as the request already asked", never
//! "downgrade below what the client asked". A durable out-of-process backend
//! behind [`SessionFloorStore`] is the planned follow-up.

use std::time::{Duration, Instant};

use dashmap::DashMap;

use super::config::Tier;

/// Entry count that triggers an expired-entry sweep during a commit.
const SWEEP_LEN: usize = 8_192;

/// Storage seam for the session floor, so a durable out-of-process adapter
/// (Redis / generic KV) can slot in behind the same contract later.
pub(crate) trait SessionFloorStore: Send + Sync {
    /// The live committed floor for `session`, if any.
    fn floor(&self, session: &str, now: Instant) -> Option<Tier>;
    /// Ratchets the floor for `session` up to at least `tier` (never down)
    /// and refreshes its inactivity TTL.
    fn commit(&self, session: &str, tier: Tier, now: Instant);
    /// Drops the floor for `session` (end-of-session eviction).
    fn evict(&self, session: &str);
}

/// A committed floor and its last-activity timestamp.
#[derive(Debug, Clone, Copy)]
struct FloorEntry {
    /// The committed tier floor.
    tier: Tier,
    /// Last commit time; the inactivity TTL counts from here.
    touched: Instant,
}

impl FloorEntry {
    /// Whether this entry's inactivity TTL has elapsed at `now`.
    fn expired(&self, now: Instant, ttl: Duration) -> bool {
        now.duration_since(self.touched) >= ttl
    }
}

/// In-process [`SessionFloorStore`] backed by a concurrent map.
///
/// Expired entries are dropped lazily on access, plus a bulk sweep whenever a
/// commit finds the map at [`SWEEP_LEN`] entries or more.
#[derive(Debug)]
pub(crate) struct InMemorySessionFloor {
    /// Committed floors keyed by session id.
    entries: DashMap<String, FloorEntry>,
    /// Inactivity window after which a floor is dropped.
    ttl: Duration,
}

impl InMemorySessionFloor {
    /// Creates an empty floor store with the given inactivity TTL.
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
        }
    }

    /// Bulk-drops expired entries once the map grows past [`SWEEP_LEN`].
    fn sweep_if_full(&self, now: Instant) {
        if self.entries.len() >= SWEEP_LEN {
            self.entries.retain(|_, entry| !entry.expired(now, self.ttl));
        }
    }
}

impl SessionFloorStore for InMemorySessionFloor {
    fn floor(&self, session: &str, now: Instant) -> Option<Tier> {
        let entry = self.entries.get(session)?;
        if entry.expired(now, self.ttl) {
            drop(entry);
            self.entries.remove(session);
            return None;
        }
        Some(entry.tier)
    }

    fn commit(&self, session: &str, tier: Tier, now: Instant) {
        self.sweep_if_full(now);
        let mut entry = self
            .entries
            .entry(session.to_owned())
            .or_insert(FloorEntry { tier, touched: now });
        if entry.expired(now, self.ttl) {
            // A dead floor must not resurrect: restart from this turn's tier.
            *entry = FloorEntry { tier, touched: now };
        } else {
            entry.tier = entry.tier.max(tier);
            entry.touched = now;
        }
    }

    fn evict(&self, session: &str) {
        self.entries.remove(session);
    }
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "unwrap/panic are acceptable in tests"
)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{InMemorySessionFloor, SessionFloorStore as _, Tier};

    /// One-hour TTL used across the tests.
    const TTL: Duration = Duration::from_secs(3_600);

    #[test]
    fn ratchet_moves_up_and_never_down() {
        let store = InMemorySessionFloor::new(TTL);
        let now = Instant::now();
        store.commit("session-a", Tier::Weak, now);
        assert_eq!(store.floor("session-a", now), Some(Tier::Weak), "weak floor committed");
        store.commit("session-a", Tier::Strong, now);
        assert_eq!(store.floor("session-a", now), Some(Tier::Strong), "upgrade committed");
        store.commit("session-a", Tier::Weak, now);
        assert_eq!(
            store.floor("session-a", now),
            Some(Tier::Strong),
            "a weak commit must not lower a strong floor"
        );
    }

    #[test]
    fn sessions_are_isolated() {
        let store = InMemorySessionFloor::new(TTL);
        let now = Instant::now();
        store.commit("session-a", Tier::Strong, now);
        assert_eq!(store.floor("session-b", now), None, "other sessions hold no floor");
    }

    #[test]
    fn eviction_drops_the_floor() {
        let store = InMemorySessionFloor::new(TTL);
        let now = Instant::now();
        store.commit("session-a", Tier::Strong, now);
        store.evict("session-a");
        assert_eq!(store.floor("session-a", now), None, "evicted session starts fresh");
    }

    #[test]
    fn ttl_expires_the_floor() {
        let store = InMemorySessionFloor::new(TTL);
        let now = Instant::now();
        store.commit("session-a", Tier::Strong, now);
        let later = now.checked_add(TTL).unwrap();
        assert_eq!(store.floor("session-a", later), None, "expired floor is gone");
        assert_eq!(
            store.floor("session-a", now),
            None,
            "the expired entry is removed, not merely hidden"
        );
    }

    #[test]
    fn commit_after_expiry_restarts_from_new_tier() {
        let store = InMemorySessionFloor::new(TTL);
        let now = Instant::now();
        store.commit("session-a", Tier::Strong, now);
        let later = now.checked_add(TTL).unwrap();
        store.commit("session-a", Tier::Weak, later);
        assert_eq!(
            store.floor("session-a", later),
            Some(Tier::Weak),
            "a dead strong floor must not resurrect past its TTL"
        );
    }

    #[test]
    fn commit_refreshes_the_ttl() {
        let store = InMemorySessionFloor::new(TTL);
        let now = Instant::now();
        store.commit("session-a", Tier::Strong, now);
        let midway = now.checked_add(TTL.checked_div(2).unwrap()).unwrap();
        store.commit("session-a", Tier::Strong, midway);
        let past_first_deadline = now.checked_add(TTL).unwrap();
        assert_eq!(
            store.floor("session-a", past_first_deadline),
            Some(Tier::Strong),
            "activity extends the floor's life"
        );
    }
}
