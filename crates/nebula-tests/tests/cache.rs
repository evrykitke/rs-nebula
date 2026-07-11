//! Proof of concept: the Redis-backed cache. Talks to the Redis from
//! docker-compose (`docker compose up -d`); skips when it is unreachable.
//! A disabled cache needs no infrastructure and is exercised inline.
//!
//! Each run uses a unique key prefix so repeated or parallel runs never
//! see each other's entries, and tidies up after itself.

use nebula::TenantRef;
use nebula::config::{CacheConfig, RedisConfig};
use nebula::Cache;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    seats: u32,
}

fn redis_config() -> RedisConfig {
    let url = std::env::var("NEBULA_TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    RedisConfig {
        url: url.as_str().into(),
    }
}

/// Connect a live cache under a unique prefix, or `None` (with a skip
/// note) when Redis is down.
async fn live_cache() -> Option<Cache> {
    let config = CacheConfig {
        enabled: true,
        prefix: format!("nebula-test-{}", Uuid::new_v4().simple()),
        default_ttl_secs: 60,
    };
    match Cache::connect(&redis_config(), &config).await {
        Ok(cache) => Some(cache),
        Err(e) => {
            eprintln!("SKIPPED: Redis is not reachable ({e}); docker compose up -d");
            None
        }
    }
}

#[tokio::test]
async fn round_trips_reads_writes_and_invalidation() {
    let Some(cache) = live_cache().await else {
        return;
    };
    let scope = cache.global();

    // A cold key misses.
    assert!(
        scope.get::<Profile>("acme").await.is_none(),
        "an unwritten key must miss"
    );

    // Write, then read back the exact value.
    let profile = Profile {
        name: "Acme".into(),
        seats: 12,
    };
    scope.set("acme", &profile, Duration::from_secs(60)).await;
    assert_eq!(
        scope.get::<Profile>("acme").await.as_ref(),
        Some(&profile),
        "a written value must round-trip"
    );

    // Invalidation removes it.
    scope.delete("acme").await;
    assert!(
        scope.get::<Profile>("acme").await.is_none(),
        "a deleted key must miss again"
    );

    scope.clear().await;
}

#[tokio::test]
async fn get_or_set_computes_once_then_caches() {
    let Some(cache) = live_cache().await else {
        return;
    };
    let scope = cache.scope("reports").expect("valid scope name");

    let calls = Arc::new(AtomicUsize::new(0));

    // First call misses and computes.
    let calls_a = calls.clone();
    let first = scope
        .get_or_set("q1", Duration::from_secs(60), || async move {
            calls_a.fetch_add(1, Ordering::SeqCst);
            Ok::<_, nebula::Error>(Profile {
                name: "Report".into(),
                seats: 7,
            })
        })
        .await
        .expect("compute must succeed");
    assert_eq!(first.seats, 7);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the miss must compute once");

    // Second call hits — compute does not run again.
    let calls_b = calls.clone();
    let second = scope
        .get_or_set("q1", Duration::from_secs(60), || async move {
            calls_b.fetch_add(1, Ordering::SeqCst);
            Ok::<_, nebula::Error>(Profile {
                name: "Report".into(),
                seats: 999,
            })
        })
        .await
        .expect("cached value must return");
    assert_eq!(second, first, "the hit must return the first, cached value");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a hit must not recompute the value"
    );

    scope.clear().await;
}

#[tokio::test]
async fn scopes_and_tenants_are_isolated() {
    let Some(cache) = live_cache().await else {
        return;
    };

    let acme = TenantRef {
        id: Uuid::new_v4(),
        name: "acme".into(),
    };
    let globex = TenantRef {
        id: Uuid::new_v4(),
        name: "globex".into(),
    };

    let value = Profile {
        name: "shared-key".into(),
        seats: 1,
    };
    // Same key, different tenants — they must not see each other.
    cache
        .tenant(&acme)
        .set("settings", &value, Duration::from_secs(60))
        .await;
    assert_eq!(
        cache.tenant(&acme).get::<Profile>("settings").await.as_ref(),
        Some(&value),
        "the writing tenant must read its own value"
    );
    assert!(
        cache
            .tenant(&globex)
            .get::<Profile>("settings")
            .await
            .is_none(),
        "another tenant must not see the value under the same key"
    );

    // clear() drops one scope only.
    cache
        .global()
        .set("keep", &value, Duration::from_secs(60))
        .await;
    cache.tenant(&acme).clear().await;
    assert!(
        cache.tenant(&acme).get::<Profile>("settings").await.is_none(),
        "clear must drop the tenant's entries"
    );
    assert_eq!(
        cache.global().get::<Profile>("keep").await.as_ref(),
        Some(&value),
        "clear must not touch other scopes"
    );

    // Invalid scope names are rejected.
    assert!(cache.scope("Bad Name").is_err());

    cache.global().clear().await;
    cache.tenant(&globex).clear().await;
}

/// A disabled cache is a transparent no-op — no Redis needed.
#[tokio::test]
async fn disabled_cache_is_a_no_op() {
    let cache = Cache::disabled(&CacheConfig {
        enabled: false,
        prefix: "unused".into(),
        default_ttl_secs: 60,
    });
    assert!(!cache.is_enabled());

    let scope = cache.global();
    scope
        .set(
            "k",
            &Profile {
                name: "x".into(),
                seats: 1,
            },
            Duration::from_secs(60),
        )
        .await;
    assert!(
        scope.get::<Profile>("k").await.is_none(),
        "a disabled cache never stores anything"
    );

    // get_or_set still returns the computed value; it just never caches.
    let calls = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        let calls = calls.clone();
        let value = scope
            .get_or_set("k", Duration::from_secs(60), || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, nebula::Error>(Profile {
                    name: "y".into(),
                    seats: 2,
                })
            })
            .await
            .unwrap();
        assert_eq!(value.seats, 2);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "with no cache, every call recomputes"
    );
}
