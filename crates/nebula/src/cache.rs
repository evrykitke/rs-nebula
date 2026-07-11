//! Redis-backed caching — a read-through store for the expensive answers
//! a request would otherwise recompute.
//!
//! A cache is an optimization, never a source of truth: correctness must
//! not depend on it. So every runtime failure degrades instead of
//! propagating — a read that can't reach Redis is a **miss**, a write
//! that can't reach Redis is **dropped** (both logged). Only a
//! misconfiguration surfaces, and only at boot: `cache.enabled` with an
//! unreachable Redis fails [`Cache::connect`], like the job queue. When
//! caching is off the whole primitive is a transparent no-op, so a module
//! calls it the same way whether or not the deployment runs Redis.
//!
//! Keys are namespaced. The kernel creates one [`Cache`] and shares it
//! with every module (and, as a request extension, with handlers);
//! callers take a [`Scope`] — [`Cache::tenant`] for per-tenant entries,
//! [`Cache::scope`] for a named area, [`Cache::global`] for the rest —
//! and every key it stores is prefixed `{cache.prefix}:{scope}:`. So a
//! tenant's entries never collide with another's, and [`Scope::clear`]
//! can drop exactly one scope without touching the rest.

use crate::config::{CacheConfig, RedisConfig};
use crate::error::{Error, Result};
use crate::tenancy::TenantRef;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// The application cache. Cheap to clone (shares one multiplexed,
/// auto-reconnecting Redis connection); one per application.
#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` when caching is disabled — every operation is then a no-op.
    manager: Option<ConnectionManager>,
    prefix: String,
    default_ttl: Duration,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("enabled", &self.inner.manager.is_some())
            .field("prefix", &self.inner.prefix)
            .finish_non_exhaustive()
    }
}

impl Cache {
    /// A disabled cache: every read misses, every write is dropped. Used
    /// when `cache.enabled` is off, so modules need no feature check.
    pub fn disabled(config: &CacheConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                manager: None,
                prefix: config.prefix.clone(),
                default_ttl: Duration::from_secs(config.default_ttl_secs.max(1)),
            }),
        }
    }

    /// Connect to Redis and return a live cache. Fails fast so a bad
    /// `redis.url` is a boot error rather than a silent stream of misses:
    /// the connection is bounded by a short timeout and a few retries,
    /// because [`ConnectionManager`]'s defaults (six retries, no connect
    /// timeout) would otherwise let a wrong host hang the boot. The kernel
    /// calls this only when `cache.enabled` is on. Once connected the
    /// manager reconnects on its own, so a *later* Redis blip degrades to
    /// misses rather than taking the process down.
    pub async fn connect(redis: &RedisConfig, config: &CacheConfig) -> Result<Self> {
        let client = redis::Client::open(redis.url.expose())
            .map_err(|e| Error::internal(format!("invalid redis.url for the cache: {e}")))?;
        let settings = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Duration::from_secs(3))
            .set_response_timeout(Duration::from_secs(3))
            .set_number_of_retries(2)
            .set_max_delay(500);
        let manager = ConnectionManager::new_with_config(client, settings)
            .await
            .map_err(|e| {
                Error::internal(format!("cache is enabled but Redis is unreachable: {e}"))
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                manager: Some(manager),
                prefix: config.prefix.clone(),
                default_ttl: Duration::from_secs(config.default_ttl_secs.max(1)),
            }),
        })
    }

    /// Whether this cache is backed by a live Redis connection.
    pub fn is_enabled(&self) -> bool {
        self.inner.manager.is_some()
    }

    /// Entries not tied to a tenant (host-level lookups, reference data).
    pub fn global(&self) -> Scope {
        Scope {
            inner: self.inner.clone(),
            namespace: "global".into(),
        }
    }

    /// A tenant's own slice of the cache — keyed by the tenant slug, so
    /// one tenant can never read or clear another's entries.
    pub fn tenant(&self, tenant: &TenantRef) -> Scope {
        Scope {
            inner: self.inner.clone(),
            namespace: format!("tenant:{}", tenant.name),
        }
    }

    /// A named area of the cache. The name must be 1-64 lowercase
    /// letters, digits, dashes or colons (colons subdivide it further).
    pub fn scope(&self, name: &str) -> Result<Scope> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b':'));
        if !ok {
            return Err(Error::Validation(format!(
                "a cache scope must be 1-64 lowercase letters, digits, dashes or colons, got {name:?}"
            )));
        }
        Ok(Scope {
            inner: self.inner.clone(),
            namespace: name.to_string(),
        })
    }
}

/// A namespaced view of the cache. All keys are stored under
/// `{prefix}:{namespace}:` and operations are confined to it.
#[derive(Clone)]
pub struct Scope {
    inner: Arc<Inner>,
    namespace: String,
}

impl Scope {
    /// The fully-qualified Redis key for a caller-supplied key.
    fn qualify(&self, key: &str) -> String {
        format!("{}:{}:{}", self.inner.prefix, self.namespace, key)
    }

    /// Fetch and deserialize a value. Answers `None` on a miss — and,
    /// deliberately, on any Redis or deserialization failure, so a cache
    /// outage looks like a cold cache rather than an error.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let manager = self.inner.manager.as_ref()?;
        let full = self.qualify(key);
        let mut conn = manager.clone();
        let raw: Option<String> = match conn.get(&full).await {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!(key = %full, error = %e, "cache read failed; treating as a miss");
                return None;
            }
        };
        let raw = raw?;
        match serde_json::from_str(&raw) {
            Ok(value) => Some(value),
            Err(e) => {
                // A stale shape from a previous version, say. Drop it so
                // the next write refreshes it, and miss for now.
                tracing::warn!(key = %full, error = %e, "cached value failed to deserialize; ignoring");
                None
            }
        }
    }

    /// Store a value with an explicit time-to-live. Best-effort: a write
    /// that can't reach Redis is logged and dropped, never surfaced.
    pub async fn set<T: Serialize>(&self, key: &str, value: &T, ttl: Duration) {
        let Some(manager) = self.inner.manager.as_ref() else {
            return;
        };
        let full = self.qualify(key);
        let payload = match serde_json::to_string(value) {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(key = %full, error = %e, "value failed to serialize; not caching");
                return;
            }
        };
        // Never let a zero TTL persist a key forever.
        let secs = ttl.as_secs().max(1);
        let mut conn = manager.clone();
        if let Err(e) = conn.set_ex::<_, _, ()>(&full, payload, secs).await {
            tracing::warn!(key = %full, error = %e, "cache write failed; dropping");
        }
    }

    /// Store a value with the configured `cache.default_ttl_secs`.
    pub async fn set_default<T: Serialize>(&self, key: &str, value: &T) {
        self.set(key, value, self.inner.default_ttl).await;
    }

    /// Read-through: return the cached value, or run `compute`, cache its
    /// result for `ttl`, and return it. `compute` runs only on a miss;
    /// its error propagates (the cache never masks a real failure) and,
    /// naturally, nothing is cached.
    pub async fn get_or_set<T, F, Fut>(&self, key: &str, ttl: Duration, compute: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if let Some(hit) = self.get::<T>(key).await {
            return Ok(hit);
        }
        let value = compute().await?;
        self.set(key, &value, ttl).await;
        Ok(value)
    }

    /// [`Scope::get_or_set`] with the configured default TTL.
    pub async fn cached<T, F, Fut>(&self, key: &str, compute: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.get_or_set(key, self.inner.default_ttl, compute).await
    }

    /// Remove one entry — invalidate after a write. Best-effort.
    pub async fn delete(&self, key: &str) {
        let Some(manager) = self.inner.manager.as_ref() else {
            return;
        };
        let full = self.qualify(key);
        let mut conn = manager.clone();
        if let Err(e) = conn.del::<_, ()>(&full).await {
            tracing::warn!(key = %full, error = %e, "cache delete failed");
        }
    }

    /// Drop every entry in this scope, and only this scope. SCANs the
    /// namespace and UNLINKs in batches (non-blocking on the server).
    /// Best-effort; a failure part-way leaves the rest in place.
    pub async fn clear(&self) {
        let Some(manager) = self.inner.manager.as_ref() else {
            return;
        };
        let pattern = format!("{}:{}:*", self.inner.prefix, self.namespace);
        let mut conn = manager.clone();
        let mut cursor: u64 = 0;
        loop {
            let scanned: redis::RedisResult<(u64, Vec<String>)> = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut conn)
                .await;
            let (next, keys) = match scanned {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!(scope = %self.namespace, error = %e, "cache clear scan failed");
                    return;
                }
            };
            if !keys.is_empty()
                && let Err(e) = redis::cmd("UNLINK")
                    .arg(&keys)
                    .query_async::<()>(&mut conn)
                    .await
            {
                tracing::warn!(scope = %self.namespace, error = %e, "cache clear unlink failed");
                return;
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
    }
}
