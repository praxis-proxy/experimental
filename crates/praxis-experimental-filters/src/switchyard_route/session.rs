//! Session key derivation and in-process last-success map.

use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    hash::Hasher as _,
    time::{Duration, Instant},
};

use http::HeaderMap;
use serde_json::Value;

use super::config::{SessionFloor, Tier};

/// HTTP header used as the session key when present and non-empty.
pub(crate) const SESSION_ID_HEADER: &str = "x-switchyard-session-id";

/// Idle time after which a remembered success is dropped.
pub(crate) const SESSION_IDLE_TTL: Duration = Duration::from_secs(1_800);

/// Maximum remembered sessions; oldest recency nodes are evicted first.
pub(crate) const SESSION_MAX_ENTRIES: usize = 10_000;

/// Recomputed each request from the header or the opening messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    /// Prefixed encoding (`h:` header, `o:` opening hash).
    inner: String,
}

impl SessionKey {
    /// Borrowed map key.
    pub(crate) fn as_str(&self) -> &str {
        &self.inner
    }
}

/// In-process map of session keys to a stored tier.
///
/// With [`SessionFloor::Enabled`] this is a one-way floor. With
/// [`SessionFloor::Disabled`] it is the last live judge success.
#[derive(Debug)]
pub(crate) struct SessionStore {
    /// Live entries keyed by [`SessionKey::as_str`].
    entries: HashMap<String, SessionEntry>,
    /// Recency list; may contain stale duplicate nodes.
    recency: VecDeque<RecencyNode>,
    /// Monotonic stamp paired with recency nodes.
    next_seq: u64,
    /// Idle TTL applied on lookup and insert.
    ttl: Duration,
    /// Hard cap on `entries.len()`.
    max_entries: usize,
}

/// Live map value: last judge success, idle timestamp, recency stamp.
#[derive(Debug, Clone, Copy)]
struct SessionEntry {
    /// Stored tier for this key (floor when enabled, last live success when disabled).
    tier: Tier,
    /// Last remember or hit time (idle TTL).
    last_touch: Instant,
    /// Matches a recency node when this entry is still the latest write.
    seq: u64,
}

/// Recency-list node; stale duplicates are skipped when `seq` no longer matches.
#[derive(Debug, Clone)]
struct RecencyNode {
    /// Copy of the map key.
    map_key: String,
    /// `SessionEntry::seq` at the time this node was pushed.
    seq: u64,
}

/// What to do with the front recency node during sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontAction {
    /// Front is the current LRU and still within TTL.
    Keep,
    /// Duplicate or already-removed node.
    DropNode,
    /// Front is the current entry and has gone idle.
    DropEntry,
}

impl SessionStore {
    /// Empty store with explicit TTL and capacity.
    pub(crate) fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            recency: VecDeque::new(),
            next_seq: 0,
            ttl,
            max_entries,
        }
    }

    /// POC defaults: 30 minutes idle, [`SESSION_MAX_ENTRIES`] cap.
    pub(crate) fn with_defaults() -> Self {
        Self::new(SESSION_IDLE_TTL, SESSION_MAX_ENTRIES)
    }

    /// Records a real judge success. Default Strong / 503 must not call this.
    ///
    /// With [`SessionFloor::Enabled`], the stored tier only rises. With
    /// [`SessionFloor::Disabled`], the new verdict replaces whatever was stored.
    pub(crate) fn remember(&mut self, key: &SessionKey, tier: Tier, now: Instant, session_floor: SessionFloor) {
        self.sweep(now);
        let seq = self.bump_seq();
        let map_key = key.as_str().to_owned();
        let effective_tier = self.tier_to_store(&map_key, tier, session_floor);

        self.entries.insert(
            map_key.clone(),
            SessionEntry {
                tier: effective_tier,
                last_touch: now,
                seq,
            },
        );
        self.recency.push_back(RecencyNode { map_key, seq });
        self.evict_over_capacity();
    }

    /// Floor on: `max(stored, incoming)`. Floor off: incoming as-is.
    fn tier_to_store(&self, map_key: &str, incoming: Tier, session_floor: SessionFloor) -> Tier {
        match session_floor {
            SessionFloor::Disabled => incoming,
            SessionFloor::Enabled => self
                .entries
                .get(map_key)
                .map_or(incoming, |existing| std::cmp::max(existing.tier, incoming)),
        }
    }

    /// Returns the remembered tier, refreshing idle TTL. Expired keys miss.
    pub(crate) fn last_success(&mut self, key: &SessionKey, now: Instant) -> Option<Tier> {
        self.sweep(now);
        if self.drop_if_idle(key, now) {
            self.maybe_compact_recency();
            return None;
        }
        let seq = self.bump_seq();
        let entry = self.entries.get_mut(key.as_str())?;
        entry.last_touch = now;
        entry.seq = seq;
        let tier = entry.tier;
        self.recency.push_back(RecencyNode {
            map_key: key.as_str().to_owned(),
            seq,
        });
        self.maybe_compact_recency();
        Some(tier)
    }

    /// Drops the key when idle TTL has elapsed. Returns whether it was dropped or absent.
    fn drop_if_idle(&mut self, key: &SessionKey, now: Instant) -> bool {
        let Some(entry) = self.entries.get(key.as_str()) else {
            return true;
        };
        if now.saturating_duration_since(entry.last_touch) <= self.ttl {
            return false;
        }
        self.entries.remove(key.as_str());
        true
    }

    /// Next recency stamp; saturates instead of wrapping.
    fn bump_seq(&mut self) -> u64 {
        self.next_seq = self.next_seq.saturating_add(1);
        self.next_seq
    }

    /// Drops stale recency nodes and idle entries from the front of the list.
    fn sweep(&mut self, now: Instant) {
        loop {
            match self.inspect_front(now) {
                None | Some(FrontAction::Keep) => break,
                Some(FrontAction::DropNode) => {
                    self.recency.pop_front();
                },
                Some(FrontAction::DropEntry) => {
                    self.drop_front_entry();
                },
            }
        }
    }

    /// Classifies the current LRU node without mutating the store.
    fn inspect_front(&self, now: Instant) -> Option<FrontAction> {
        let front = self.recency.front()?;
        let Some(entry) = self.entries.get(&front.map_key) else {
            return Some(FrontAction::DropNode);
        };
        if entry.seq != front.seq {
            return Some(FrontAction::DropNode);
        }
        if now.saturating_duration_since(entry.last_touch) > self.ttl {
            return Some(FrontAction::DropEntry);
        }
        Some(FrontAction::Keep)
    }

    /// Removes the front recency node and its live map entry.
    fn drop_front_entry(&mut self) {
        if let Some(node) = self.recency.pop_front() {
            self.entries.remove(&node.map_key);
        }
    }

    /// Pops LRU live entries until `entries` fits in `max_entries`.
    fn evict_over_capacity(&mut self) {
        while self.entries.len() > self.max_entries {
            let Some(node) = self.recency.pop_front() else {
                break;
            };
            let current = self
                .entries
                .get(&node.map_key)
                .is_some_and(|entry| entry.seq == node.seq);
            if current {
                self.entries.remove(&node.map_key);
            }
        }
        self.maybe_compact_recency();
    }

    /// Rebuilds `recency` when stale duplicates outgrow live entries.
    fn maybe_compact_recency(&mut self) {
        let limit = self.entries.len().saturating_mul(2);
        if self.recency.len() <= limit {
            return;
        }
        self.compact_recency();
    }

    /// Drops recency nodes whose `seq` is no longer the live entry.
    fn compact_recency(&mut self) {
        let entries = &self.entries;
        self.recency
            .retain(|node| entries.get(&node.map_key).is_some_and(|entry| entry.seq == node.seq));
    }
}

/// Header if non-empty, otherwise system + first user message hash.
pub(crate) fn session_key_from_request(headers: &HeaderMap, body: &Value) -> Option<SessionKey> {
    header_session_key(headers).or_else(|| opening_session_key(body))
}

/// Uses `x-switchyard-session-id` when it is present and non-empty.
fn header_session_key(headers: &HeaderMap) -> Option<SessionKey> {
    let raw = headers.get(SESSION_ID_HEADER)?.to_str().ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let limited: String = trimmed.chars().take(256).collect();
    Some(SessionKey {
        inner: format!("h:{limited}"),
    })
}

/// Hashes the system prompt (optional) and the first non-empty user message.
fn opening_session_key(body: &Value) -> Option<SessionKey> {
    let messages = body.get("messages")?.as_array()?;
    let first_user = first_role_text(messages, "user").filter(|text| !text.is_empty())?;
    let system = first_role_text(messages, "system").unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    write_len_prefixed(&mut hasher, system.as_bytes());
    write_len_prefixed(&mut hasher, first_user.as_bytes());
    Some(SessionKey {
        inner: format!("o:{:016X}", hasher.finish()),
    })
}

/// Writes `len` then `bytes` so adjacent fields cannot alias under concatenation.
fn write_len_prefixed(hasher: &mut DefaultHasher, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.write_u64(len);
    hasher.write(bytes);
}

/// First `messages[]` entry with this `role` that has extractable text.
fn first_role_text(messages: &[Value], role: &str) -> Option<String> {
    messages.iter().find_map(|message| message_text_if_role(message, role))
}

/// Text for one chat message when its role matches.
fn message_text_if_role(message: &Value, role: &str) -> Option<String> {
    let object = message.as_object()?;
    if object.get("role")?.as_str()? != role {
        return None;
    }
    content_text(object.get("content")?)
}

/// String `content` or concatenated `text` parts of a content array.
fn content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    if texts.is_empty() { None } else { Some(texts.join("")) }
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "unwrap/expect/panic/indexing/length are acceptable in tests"
)]
mod tests {
    use std::time::{Duration, Instant};

    use http::{HeaderMap, HeaderValue};
    use serde_json::json;

    use super::{SESSION_ID_HEADER, SessionKey, SessionStore, session_key_from_request};
    use crate::switchyard_route::config::{SessionFloor, Tier};

    fn header_map(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(SESSION_ID_HEADER, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn chat(system: Option<&str>, users: &[&str]) -> serde_json::Value {
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(json!({"role": "system", "content": system}));
        }
        for user in users {
            messages.push(json!({"role": "user", "content": user}));
        }
        json!({"model": "agent-default", "messages": messages})
    }

    #[test]
    fn header_wins_over_opening_messages() {
        let body = chat(None, &["hello"]);
        let from_header = session_key_from_request(&header_map("chat-1"), &body).unwrap();
        let from_body = session_key_from_request(&HeaderMap::new(), &body).unwrap();
        assert_ne!(from_header, from_body, "header key must not equal opening hash");
        assert!(from_header.as_str().starts_with("h:"), "header keys use h: prefix");
    }

    #[test]
    fn blank_header_falls_through_to_opening() {
        let body = chat(None, &["hello"]);
        let blank = session_key_from_request(&header_map("   "), &body).unwrap();
        let opening = session_key_from_request(&HeaderMap::new(), &body).unwrap();
        assert_eq!(blank, opening, "whitespace-only header must be ignored");
    }

    #[test]
    fn later_turns_keep_the_opening_user_message() {
        let turn_one = chat(Some("be brief"), &["capital of France?"]);
        let turn_two = json!({
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "capital of France?"},
                {"role": "assistant", "content": "Paris"},
                {"role": "user", "content": "and Germany?"}
            ]
        });
        let key_one = session_key_from_request(&HeaderMap::new(), &turn_one).unwrap();
        let key_two = session_key_from_request(&HeaderMap::new(), &turn_two).unwrap();
        assert_eq!(key_one, key_two, "follow-up history must keep the opening key");
    }

    #[test]
    fn different_openings_are_different_keys() {
        let france = session_key_from_request(&HeaderMap::new(), &chat(None, &["France"])).unwrap();
        let spain = session_key_from_request(&HeaderMap::new(), &chat(None, &["Spain"])).unwrap();
        assert_ne!(france, spain, "distinct first user lines must not share a key");
    }

    #[test]
    fn system_prompt_is_part_of_the_opening_key() {
        let with_a = session_key_from_request(&HeaderMap::new(), &chat(Some("A"), &["hi"])).unwrap();
        let with_b = session_key_from_request(&HeaderMap::new(), &chat(Some("B"), &["hi"])).unwrap();
        assert_ne!(with_a, with_b, "different system prompts must not share a key");
    }

    #[test]
    fn opening_hash_does_not_alias_on_concatenated_fields() {
        let split_left = session_key_from_request(&HeaderMap::new(), &chat(Some("ab"), &["c"])).unwrap();
        let split_right = session_key_from_request(&HeaderMap::new(), &chat(Some("a"), &["bc"])).unwrap();
        assert_ne!(
            split_left, split_right,
            "system+user must be length-prefixed so concatenations do not collide"
        );
    }

    #[test]
    fn missing_user_message_has_no_opening_key() {
        let body = json!({"messages": [{"role": "system", "content": "only system"}]});
        assert!(
            session_key_from_request(&HeaderMap::new(), &body).is_none(),
            "system-only bodies are not a session"
        );
    }

    #[test]
    fn multimodal_user_text_parts_join() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello "},
                    {"type": "text", "text": "world"}
                ]
            }]
        });
        let joined = session_key_from_request(&HeaderMap::new(), &body).unwrap();
        let plain = session_key_from_request(&HeaderMap::new(), &chat(None, &["hello world"])).unwrap();
        assert_eq!(joined, plain, "text parts must hash like a single string");
    }

    #[test]
    fn remember_then_lookup_returns_tier() {
        let mut store = SessionStore::new(Duration::from_secs(60), 8);
        let key = SessionKey {
            inner: "h:demo".to_owned(),
        };
        let now = Instant::now();
        store.remember(&key, Tier::Strong, now, SessionFloor::Enabled);
        assert_eq!(store.last_success(&key, now), Some(Tier::Strong));
    }

    #[test]
    fn idle_ttl_expires_the_entry() {
        let mut store = SessionStore::new(Duration::from_secs(10), 8);
        let key = SessionKey {
            inner: "h:ttl".to_owned(),
        };
        let start = Instant::now();
        store.remember(&key, Tier::Weak, start, SessionFloor::Enabled);
        let later = start + Duration::from_secs(11);
        assert_eq!(store.last_success(&key, later), None, "idle TTL must miss");
    }

    #[test]
    fn capacity_evicts_least_recent_key() {
        let mut store = SessionStore::new(Duration::from_secs(60), 2);
        let key_a = SessionKey {
            inner: "h:a".to_owned(),
        };
        let key_b = SessionKey {
            inner: "h:b".to_owned(),
        };
        let key_c = SessionKey {
            inner: "h:c".to_owned(),
        };
        let now = Instant::now();
        store.remember(&key_a, Tier::Weak, now, SessionFloor::Enabled);
        store.remember(&key_b, Tier::Weak, now, SessionFloor::Enabled);
        store.remember(&key_c, Tier::Strong, now, SessionFloor::Enabled);
        assert_eq!(store.last_success(&key_a, now), None, "oldest key must be evicted");
        assert_eq!(store.last_success(&key_b, now), Some(Tier::Weak));
        assert_eq!(store.last_success(&key_c, now), Some(Tier::Strong));
    }

    #[test]
    fn lookup_refreshes_recency_before_capacity_eviction() {
        let mut store = SessionStore::new(Duration::from_secs(60), 2);
        let key_a = SessionKey {
            inner: "h:a".to_owned(),
        };
        let key_b = SessionKey {
            inner: "h:b".to_owned(),
        };
        let key_c = SessionKey {
            inner: "h:c".to_owned(),
        };
        let now = Instant::now();
        store.remember(&key_a, Tier::Strong, now, SessionFloor::Enabled);
        store.remember(&key_b, Tier::Weak, now, SessionFloor::Enabled);
        assert_eq!(store.last_success(&key_a, now), Some(Tier::Strong));
        store.remember(&key_c, Tier::Weak, now, SessionFloor::Enabled);
        assert_eq!(
            store.last_success(&key_a, now),
            Some(Tier::Strong),
            "touched key must survive a later insert"
        );
        assert_eq!(store.last_success(&key_b, now), None, "untouched LRU must be evicted");
    }

    #[test]
    fn remember_upgrades_tier_from_weak_to_strong() {
        let mut store = SessionStore::new(Duration::from_secs(60), 1);
        let key = SessionKey {
            inner: "h:one".to_owned(),
        };
        let now = Instant::now();
        store.remember(&key, Tier::Weak, now, SessionFloor::Enabled);
        store.remember(&key, Tier::Strong, now, SessionFloor::Enabled);
        assert_eq!(store.last_success(&key, now), Some(Tier::Strong));
        assert_eq!(store.entries.len(), 1, "overwrite must not add a second live entry");
    }

    #[test]
    fn remember_does_not_downgrade_tier_from_strong_to_weak() {
        let mut store = SessionStore::new(Duration::from_secs(60), 8);
        let key = SessionKey {
            inner: "h:floor".to_owned(),
        };
        let now = Instant::now();
        store.remember(&key, Tier::Strong, now, SessionFloor::Enabled);
        store.remember(&key, Tier::Weak, now, SessionFloor::Enabled);
        assert_eq!(
            store.last_success(&key, now),
            Some(Tier::Strong),
            "floor semantics: Strong must not downgrade to Weak"
        );
    }

    #[test]
    fn remember_replaces_tier_when_floor_disabled() {
        let mut store = SessionStore::new(Duration::from_secs(60), 8);
        let key = SessionKey {
            inner: "h:no-floor".to_owned(),
        };
        let now = Instant::now();
        store.remember(&key, Tier::Strong, now, SessionFloor::Disabled);
        store.remember(&key, Tier::Weak, now, SessionFloor::Disabled);
        assert_eq!(
            store.last_success(&key, now),
            Some(Tier::Weak),
            "disabled floor must store the latest live verdict"
        );
    }

    #[test]
    fn recency_compacts_when_one_key_is_hit_repeatedly() {
        let mut store = SessionStore::new(Duration::from_secs(60), 8);
        let quiet = SessionKey {
            inner: "h:quiet".to_owned(),
        };
        let hot = SessionKey {
            inner: "h:hot".to_owned(),
        };
        let now = Instant::now();
        store.remember(&quiet, Tier::Weak, now, SessionFloor::Enabled);
        store.remember(&hot, Tier::Strong, now, SessionFloor::Enabled);
        for _ in 0..40 {
            assert_eq!(store.last_success(&hot, now), Some(Tier::Strong));
        }
        assert!(
            store.recency.len() <= store.entries.len().saturating_mul(2),
            "recency must stay bounded, got {}",
            store.recency.len()
        );
        assert_eq!(
            store.last_success(&quiet, now),
            Some(Tier::Weak),
            "idle key must survive compaction"
        );
    }
}
