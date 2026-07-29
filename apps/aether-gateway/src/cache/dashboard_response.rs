use std::time::Duration;

use aether_cache::ExpiringMap;

const MAX_ENTRIES: usize = 256;

#[derive(Debug)]
pub(crate) struct DashboardResponseCache {
    entries: ExpiringMap<String, Vec<u8>>,
}

impl Default for DashboardResponseCache {
    fn default() -> Self {
        Self {
            entries: ExpiringMap::new(),
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

    pub(crate) fn clear(&self) {
        self.entries.clear();
    }
}
