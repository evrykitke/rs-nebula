//! Domain and integration events — how bounded contexts react to each
//! other without depending on each other.
//!
//! An [`Event`] is a fact that already happened, named in past tense
//! (`account.user_registered`). Modules subscribe during `configure`
//! through [`ModuleContext::events`](crate::module::ModuleContext::events)
//! and publish from handlers or services:
//!
//! - [`Events::publish`] — **domain event**, in-process. Every subscriber
//!   in this process runs, awaited in subscription order. Handler errors
//!   are logged, never propagated: the fact has already happened and a
//!   failing reaction must not un-happen it.
//! - [`Events::broadcast`] — **integration event**, via RabbitMQ when
//!   `events.distributed` is on. The event goes to a topic exchange with
//!   its name as the routing key; every service's queue bound to that
//!   name gets one copy, and one instance per service processes it —
//!   including this one, so subscribers still run exactly once per
//!   service. With distributed events off, `broadcast` degrades to an
//!   in-process `publish`, so single-node deployments need no broker.
//!
//! Subscriptions must be in place before the consumer starts (the kernel
//! guarantees this: modules configure, then the broker connects, then
//! `serve` starts the consumer), because queue bindings are derived from
//! the subscribed event names.

mod remote;

use crate::error::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};

pub use remote::Remote;

/// A fact that happened in a bounded context. `Clone` because every
/// subscriber gets its own copy; serde because integration events cross
/// the wire as JSON.
pub trait Event: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    /// Dot-separated, past-tense: `"account.user_registered"`. Doubles
    /// as the AMQP routing key for broadcast events.
    const NAME: &'static str;
}

type BoxedFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type Handler = Arc<dyn Fn(Arc<dyn Any + Send + Sync>) -> BoxedFuture + Send + Sync>;
type Decoder = Arc<dyn Fn(&[u8]) -> serde_json::Result<Arc<dyn Any + Send + Sync>> + Send + Sync>;

struct Subscription {
    handlers: Vec<Handler>,
    /// Turns a broker payload back into the typed event for dispatch.
    decode: Decoder,
}

#[derive(Default)]
struct Inner {
    subscriptions: RwLock<HashMap<&'static str, Subscription>>,
    remote: OnceLock<Remote>,
}

/// The event bus. Cheap to clone; one per application, created by the
/// kernel and shared with every module (and, as a request extension,
/// with application handlers).
#[derive(Clone, Default)]
pub struct Events {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events").finish_non_exhaustive()
    }
}

impl Events {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to an event type. The handler runs for every local
    /// `publish` of `E` and for every copy of `E` this service receives
    /// from the broker.
    pub fn subscribe<E, F, Fut>(&self, handler: F)
    where
        E: Event,
        F: Fn(E) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |any: Arc<dyn Any + Send + Sync>| {
            let event = any
                .downcast_ref::<E>()
                .expect("events are routed to handlers by name")
                .clone();
            Box::pin(handler(event))
        });
        let mut subs = self.inner.subscriptions.write().expect("event bus lock");
        subs.entry(E::NAME)
            .or_insert_with(|| Subscription {
                handlers: Vec::new(),
                decode: Arc::new(|bytes| {
                    serde_json::from_slice::<E>(bytes)
                        .map(|event| Arc::new(event) as Arc<dyn Any + Send + Sync>)
                }),
            })
            .handlers
            .push(handler);
    }

    /// Publish a domain event to this process's subscribers. Handlers
    /// run sequentially; a failing handler is logged and the rest still
    /// run — the fact has already happened.
    pub async fn publish<E: Event>(&self, event: E) {
        self.dispatch(E::NAME, Arc::new(event)).await;
    }

    /// Publish an integration event: to the broker when distributed
    /// events are enabled (subscribers here receive it through this
    /// service's queue), otherwise in-process like [`Events::publish`].
    /// The error means the broker refused or lost the message.
    pub async fn broadcast<E: Event>(&self, event: E) -> Result<()> {
        match self.inner.remote.get() {
            Some(remote) => {
                let payload = serde_json::to_vec(&event).map_err(|e| {
                    crate::error::Error::internal(format!(
                        "failed to serialize event {}: {e}",
                        E::NAME
                    ))
                })?;
                remote.publish(E::NAME, payload).await
            }
            None => {
                self.publish(event).await;
                Ok(())
            }
        }
    }

    async fn dispatch(&self, name: &str, event: Arc<dyn Any + Send + Sync>) {
        let handlers = {
            let subs = self.inner.subscriptions.read().expect("event bus lock");
            subs.get(name).map(|s| s.handlers.clone()).unwrap_or_default()
        };
        tracing::debug!(event = name, handlers = handlers.len(), "publishing event");
        for handler in handlers {
            if let Err(e) = handler(event.clone()).await {
                tracing::error!(event = name, error = %e, "event handler failed");
            }
        }
    }

    /// Decode and dispatch a broker delivery. Unknown names (bindings
    /// left over from a previous deployment) and undecodable payloads
    /// are logged and dropped rather than requeued into a poison loop.
    async fn dispatch_raw(&self, name: &str, payload: &[u8]) {
        let decode = {
            let subs = self.inner.subscriptions.read().expect("event bus lock");
            subs.get(name).map(|s| s.decode.clone())
        };
        let Some(decode) = decode else {
            tracing::warn!(event = name, "received an event nobody subscribes to; dropping");
            return;
        };
        match decode(payload) {
            Ok(event) => self.dispatch(name, event).await,
            Err(e) => {
                tracing::error!(event = name, error = %e, "failed to decode event payload; dropping")
            }
        }
    }

    /// The event names this service subscribes to — the queue bindings.
    fn subscribed_names(&self) -> Vec<&'static str> {
        let subs = self.inner.subscriptions.read().expect("event bus lock");
        subs.keys().copied().collect()
    }

    /// Connect the bus to RabbitMQ: declares the topic exchange and this
    /// service's durable queue, and binds every subscribed event name.
    /// The kernel calls this when `events.distributed` is on; fails fast
    /// so a broken broker is a boot error, not a silent message drop.
    pub async fn connect(
        &self,
        rabbitmq: &crate::config::RabbitMqConfig,
        config: &crate::config::EventsConfig,
    ) -> Result<()> {
        let remote = Remote::connect(rabbitmq, config, self.subscribed_names()).await?;
        self.inner
            .remote
            .set(remote)
            .map_err(|_| crate::error::Error::internal("the event bus is already connected"))?;
        Ok(())
    }

    /// Start consuming this service's queue, dispatching deliveries to
    /// the local subscribers. Idempotent; a no-op until [`Events::connect`]
    /// has run. Answers whether a consumer was started.
    pub fn start_consumer(&self) -> bool {
        let Some(remote) = self.inner.remote.get() else {
            return false;
        };
        if !remote.claim_consumer() {
            return false;
        }
        let bus = self.clone();
        let remote = remote.clone();
        tokio::spawn(async move { remote.consume(bus).await });
        true
    }
}
