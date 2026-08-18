use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use aether_cache::ExpiringMap;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const MAX_ENTRIES: usize = 256;

#[derive(Debug)]
pub(crate) struct DashboardResponseCache {
    entries: ExpiringMap<String, Vec<u8>>,
    inflight: std::sync::Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
}

impl Default for DashboardResponseCache {
    fn default() -> Self {
        Self {
            entries: ExpiringMap::new(),
            inflight: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl DashboardResponseCache {
    pub(crate) fn get(&self, key: &str, ttl: Duration) -> Option<Vec<u8>> {
        self.entries.get_fresh(&key.to_string(), ttl)
    }

    pub(crate) fn insert(&self, key: String, value: Vec<u8>, ttl: Duration) {
        self.entries.insert(key, value, ttl, MAX_ENTRIES);
    }

    pub(crate) async fn acquire(&self, key: &str) -> OwnedMutexGuard<()> {
        let lock = match self.inflight.lock() {
            Ok(mut inflight) => {
                inflight.retain(|_, lock| lock.strong_count() > 0);
                if let Some(lock) = inflight.get(key).and_then(Weak::upgrade) {
                    lock
                } else {
                    let lock = Arc::new(AsyncMutex::new(()));
                    inflight.insert(key.to_string(), Arc::downgrade(&lock));
                    lock
                }
            }
            Err(_) => Arc::new(AsyncMutex::new(())),
        };
        lock.lock_owned().await
    }

    pub(crate) fn clear(&self) {
        self.entries.clear();
        if let Ok(mut inflight) = self.inflight.lock() {
            inflight.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_serializes_same_key_without_blocking_other_keys() {
        let cache = Arc::new(DashboardResponseCache::default());
        let first = cache.acquire("same").await;

        let waiting_cache = Arc::clone(&cache);
        let waiting = tokio::spawn(async move { waiting_cache.acquire("same").await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        let other = tokio::time::timeout(Duration::from_secs(1), cache.acquire("other"))
            .await
            .expect("another cache key should not be blocked");
        drop(other);

        drop(first);
        let acquired = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("waiting request should continue after the first finishes")
            .expect("waiting task should not panic");
        drop(acquired);

        let cleanup = cache.acquire("cleanup").await;
        let inflight = cache.inflight.lock().expect("inflight map should lock");
        assert_eq!(inflight.len(), 1);
        assert!(inflight.contains_key("cleanup"));
        drop(inflight);
        drop(cleanup);
    }
}
