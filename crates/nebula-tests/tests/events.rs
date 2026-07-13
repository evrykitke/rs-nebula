//! Proof of concept: the event bus. In-process domain events need no
//! infrastructure; the distributed round-trip talks to the RabbitMQ from
//! docker-compose; the kernel test proves a module can subscribe to
//! another context's events during configure.

use nebula::config::{Config, DatabaseConfig, EventsConfig, RabbitMqConfig};
use nebula::{Event, Events};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    run: Uuid,
    amount: usize,
}

impl Event for Ping {
    const NAME: &'static str = "nebula_tests.ping";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Unheard;

impl Event for Unheard {
    const NAME: &'static str = "nebula_tests.unheard";
}

#[tokio::test]
async fn local_bus_dispatches_to_every_subscriber() {
    let events = Events::new();
    let counter = Arc::new(AtomicUsize::new(0));

    let c = counter.clone();
    events.subscribe(move |ping: Ping| {
        let c = c.clone();
        async move {
            c.fetch_add(ping.amount, Ordering::SeqCst);
            Ok(())
        }
    });
    // A failing handler must not starve the ones after it.
    events.subscribe(|_: Ping| async { Err(nebula::Error::internal("handler goes boom")) });
    let c = counter.clone();
    events.subscribe(move |ping: Ping| {
        let c = c.clone();
        async move {
            c.fetch_add(ping.amount * 10, Ordering::SeqCst);
            Ok(())
        }
    });

    let run = Uuid::new_v4();
    events.publish(Ping { run, amount: 3 }).await;
    assert_eq!(counter.load(Ordering::SeqCst), 33, "both counting handlers must run");

    // No subscribers is a quiet no-op, and without a broker `broadcast`
    // degrades to the same in-process delivery.
    events.publish(Unheard).await;
    events.broadcast(Ping { run, amount: 1 }).await.unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 44);
}

/// Two buses with their own queues on the shared exchange — two services
/// of one deployment. A broadcast from one reaches both, each exactly
/// once, including the publisher itself.
#[tokio::test]
async fn distributed_round_trip_reaches_every_service() {
    let url = std::env::var("NEBULA_TEST_AMQP_URL")
        .unwrap_or_else(|_| "amqp://nebula:nebula_dev@127.0.0.1:5672".into());
    let rabbitmq = RabbitMqConfig {
        url: url.as_str().into(),
    };
    let config_for = |queue: &str| EventsConfig {
        distributed: true,
        exchange: "nebula-tests.events".into(),
        queue: queue.into(),
    };

    let run = Uuid::new_v4();
    let make_bus = |marker: usize, counter: Arc<AtomicUsize>| {
        let bus = Events::new();
        bus.subscribe(move |ping: Ping| {
            let counter = counter.clone();
            async move {
                if ping.run == run {
                    counter.fetch_add(ping.amount * marker, Ordering::SeqCst);
                }
                Ok(())
            }
        });
        bus
    };

    let count_a = Arc::new(AtomicUsize::new(0));
    let count_b = Arc::new(AtomicUsize::new(0));
    let bus_a = make_bus(1, count_a.clone());
    let bus_b = make_bus(100, count_b.clone());

    if let Err(e) = bus_a.connect(&rabbitmq, &config_for("nebula-tests-a")).await {
        eprintln!("SKIPPED: RabbitMQ is not reachable ({e}); docker compose up -d");
        return;
    }
    bus_b
        .connect(&rabbitmq, &config_for("nebula-tests-b"))
        .await
        .expect("second bus must connect");
    assert!(bus_a.start_consumer(), "consumer must start once connected");
    assert!(!bus_a.start_consumer(), "starting twice is a no-op");
    assert!(bus_b.start_consumer());

    bus_a.broadcast(Ping { run, amount: 7 }).await.expect("broadcast must confirm");

    let mut waited = Duration::ZERO;
    while (count_a.load(Ordering::SeqCst) < 7 || count_b.load(Ordering::SeqCst) < 700)
        && waited < Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert_eq!(count_a.load(Ordering::SeqCst), 7, "the publishing service must receive its own broadcast once");
    assert_eq!(count_b.load(Ordering::SeqCst), 700, "the other service must receive the broadcast once");
}

/// A module reacts to the account context's `UserRegistered` without the
/// account module knowing it exists.
#[tokio::test]
async fn modules_subscribe_to_other_contexts() {
    use axum::body::Body;
    use axum::http::Request;
    use nebula::account::events::UserRegistered;
    use nebula::{AccountModule, Kernel, Module, ModuleContext};
    use tower::ServiceExt;

    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let admin_db = nebula::db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");
    sea_orm::ConnectionTrait::execute_unprepared(
        &admin_db,
        "DROP TABLE IF EXISTS user_directory; DROP TABLE IF EXISTS currencies; DROP TABLE IF EXISTS audit_logs; DROP TABLE IF EXISTS permission_grants; \
         DROP TABLE IF EXISTS user_roles; DROP TABLE IF EXISTS roles; \
         DROP TABLE IF EXISTS refresh_tokens; DROP TABLE IF EXISTS users; \
         DROP TABLE IF EXISTS tenants; DROP TABLE IF EXISTS nebula_migrations;",
    )
    .await
    .expect("cleanup must work");

    struct WelcomeModule {
        registered: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Module for WelcomeModule {
        fn name(&self) -> &'static str {
            "welcome"
        }

        fn depends_on(&self) -> Vec<Box<dyn Module>> {
            vec![Box::new(AccountModule)]
        }

        fn configure(&self, ctx: &mut ModuleContext) {
            let registered = self.registered.clone();
            ctx.events().subscribe(move |event: UserRegistered| {
                let registered = registered.clone();
                async move {
                    registered.lock().unwrap().push(event.email);
                    Ok(())
                }
            });
        }
    }

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;
    // This test asserts against the main database; no per-tenant database.
    config.multitenancy.provision_databases = false;
    config.multitenancy.allow_shared_database = true;
    config.auth.jwt_secret = "test-secret-not-for-production".into();

    let registered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = Kernel::builder()
        .with_config(config)
        .add_module(WelcomeModule {
            registered: registered.clone(),
        })
        .build()
        .unwrap()
        .init()
        .await
        .expect("boot must succeed");

    let response = app
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "tenant_name": "eventsco",
                        "email": "founder@eventsco.test",
                        "password": "hunter2hunter2",
                        "first_name": "Ev",
                        "last_name": "Ents",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        *registered.lock().unwrap(),
        vec!["founder@eventsco.test".to_string()],
        "the welcome module must see the registration"
    );
}
