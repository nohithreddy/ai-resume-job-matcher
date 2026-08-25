use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

/// Simple in-memory idempotency store for POST /resumes and /jobs.
/// Keyed by (user_id, idempotency_key). Response is cached as (status_code, body_json) for replay.
/// Entries expire after TTL to avoid unbounded growth.

#[derive(Clone)]
struct Entry {
    response_status: u16,
    response_body: String,
    created_at: Instant,
}

pub struct IdempotencyStore {
    ttl: Duration,
    entries: Mutex<HashMap<(Uuid, String), Entry>>,
    max_entries: usize,
}

impl IdempotencyStore {
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_seconds.max(60)),
            entries: Mutex::new(HashMap::new()),
            max_entries: 10_000,
        }
    }

    pub fn get(&self, user_id: Uuid, key: &str) -> Option<(u16, String)> {
        let mut map = self.entries.lock().expect("idempotency lock poisoned");
        // Evict expired entries on read
        let now = Instant::now();
        map.retain(|_, entry| now.duration_since(entry.created_at) < self.ttl);
        map.get(&(user_id, key.to_owned()))
            .map(|entry| (entry.response_status, entry.response_body.clone()))
    }

    pub fn insert(&self, user_id: Uuid, key: String, status: u16, body: String) {
        let mut map = self.entries.lock().expect("idempotency lock poisoned");
        let now = Instant::now();
        map.retain(|_, entry| now.duration_since(entry.created_at) < self.ttl);
        if map.len() >= self.max_entries {
            // Drop oldest 10%
            let mut entries: Vec<_> = map.iter().map(|(k, v)| (k.clone(), v.created_at)).collect();
            entries.sort_by_key(|(_, t)| *t);
            let to_drop = entries.len() / 10 + 1;
            for (k, _) in entries.into_iter().take(to_drop) {
                map.remove(&k);
            }
        }
        map.insert(
            (user_id, key),
            Entry {
                response_status: status,
                response_body: body,
                created_at: now,
            },
        );
    }
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::new(3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn cache_and_retrieve() {
        let store = IdempotencyStore::new(60);
        let user = Uuid::now_v7();
        assert!(store.get(user, "key-1").is_none());
        store.insert(user, "key-1".to_owned(), 201, r#"{"id":"abc"}"#.to_owned());
        let cached = store.get(user, "key-1").expect("should be cached");
        assert_eq!(cached.0, 201);
        assert!(cached.1.contains("abc"));
    }
}
