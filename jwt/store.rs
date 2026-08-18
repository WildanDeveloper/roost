use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// In-memory short-TTL store for single-use token ids (download/upload)
/// and websocket revocation entries (jti/user denial timestamps).
#[derive(Default)]
pub struct TokenStore {
    /// unique_id -> expiration
    used: RwLock<HashMap<String, Instant>>,
    /// "jti:<jti>" -> issued-at floor
    denied_jti: RwLock<HashMap<String, i64>>,
    /// user uuid -> issued-at floor
    denied_user: RwLock<HashMap<String, i64>>,
    ttl: Duration,
}

impl TokenStore {
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Default::default()
        }
    }

    /// Mark a unique_id as used. Returns false if it was already used.
    pub async fn claim(&self, uid: &str) -> bool {
        let mut used = self.used.write().unwrap();
        used.retain(|_, exp| *exp > Instant::now());
        if used.contains_key(uid) {
            return false;
        }
        used.insert(uid.to_string(), Instant::now() + self.ttl);
        true
    }

    pub async fn deny_jti(&self, jti: &str, at: i64) {
        let mut m = self.denied_jti.write().unwrap();
        m.retain(|_, floor| *floor >= revocation_floor(self.ttl));
        m.insert(format!("jti:{jti}"), at);
    }

    pub async fn deny_user(&self, user: &str, at: i64) {
        let mut m = self.denied_user.write().unwrap();
        m.retain(|_, floor| *floor >= revocation_floor(self.ttl));
        m.insert(user.to_string(), at);
    }

    #[allow(dead_code)]
pub fn is_jti_denied(&self, jti: &str, iat: i64) -> bool {
        self.denied_jti
            .read()
            .unwrap()
            .get(&format!("jti:{jti}"))
            .map(|at| iat < *at)
            .unwrap_or(false)
    }

    #[allow(dead_code)]
pub fn is_user_denied(&self, user: &str, iat: i64) -> bool {
        self.denied_user
            .read()
            .unwrap()
            .get(user)
            .map(|at| iat < *at)
            .unwrap_or(false)
    }
}

/// Denial entries older than this are dropped on insert: a revocation
/// floor `at` only matters for tokens issued after it, and any token with
/// iat older than one TTL is already expired, so the floor is no longer
/// load-bearing (keeps the maps bounded).
fn revocation_floor(ttl: Duration) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now.saturating_sub(ttl.as_secs() as i64)
}