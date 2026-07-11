//! The RabbitMQ leg of the event bus: one durable topic exchange for
//! the whole deployment, one durable queue per service (`events.queue`),
//! bound to the event names the service subscribes to. Instances of the
//! same service share the queue, so each broadcast is processed once per
//! service.

use crate::config::{EventsConfig, RabbitMqConfig};
use crate::error::{Error, Result};
use futures_lite::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, ConfirmSelectOptions,
    ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Connection, ConnectionProperties, ExchangeKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A live broker attachment. Cheap to clone; owned by the event bus.
#[derive(Clone)]
pub struct Remote {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    connection: Connection,
    publisher: lapin::Channel,
    exchange: String,
    queue: String,
    consumer_started: AtomicBool,
}

fn broker_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::internal(format!("event broker: {what}: {e}"))
}

impl Remote {
    /// Connect, declare the exchange and queue, and bind the subscribed
    /// event names. Publisher confirms are on so [`Remote::publish`] can
    /// report a lost message instead of pretending.
    pub(crate) async fn connect(
        rabbitmq: &RabbitMqConfig,
        config: &EventsConfig,
        subscribed: Vec<&'static str>,
    ) -> Result<Self> {
        let connection =
            Connection::connect(rabbitmq.url.expose(), ConnectionProperties::default())
                .await
                .map_err(|e| {
                    broker_error("distributed events are enabled but RabbitMQ is unreachable", e)
                })?;
        let publisher = connection
            .create_channel()
            .await
            .map_err(|e| broker_error("failed to open the publisher channel", e))?;
        publisher
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(|e| broker_error("failed to enable publisher confirms", e))?;
        publisher
            .exchange_declare(
                config.exchange.as_str().into(),
                ExchangeKind::Topic,
                ExchangeDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| broker_error("failed to declare the exchange", e))?;
        publisher
            .queue_declare(
                config.queue.as_str().into(),
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| broker_error("failed to declare the queue", e))?;
        for name in &subscribed {
            publisher
                .queue_bind(
                    config.queue.as_str().into(),
                    config.exchange.as_str().into(),
                    (*name).into(),
                    QueueBindOptions::default(),
                    FieldTable::default(),
                )
                .await
                .map_err(|e| broker_error("failed to bind the queue", e))?;
        }
        tracing::info!(
            exchange = %config.exchange,
            queue = %config.queue,
            bindings = subscribed.len(),
            "distributed events connected to rabbitmq"
        );
        Ok(Self {
            inner: Arc::new(RemoteInner {
                connection,
                publisher,
                exchange: config.exchange.clone(),
                queue: config.queue.clone(),
                consumer_started: AtomicBool::new(false),
            }),
        })
    }

    /// Publish to the exchange and wait for the broker's confirm.
    pub(crate) async fn publish(&self, name: &str, payload: Vec<u8>) -> Result<()> {
        let confirm = self
            .inner
            .publisher
            .basic_publish(
                self.inner.exchange.as_str().into(),
                name.into(),
                BasicPublishOptions::default(),
                &payload,
                BasicProperties::default()
                    .with_content_type("application/json".into())
                    .with_delivery_mode(2), // persistent
            )
            .await
            .map_err(|e| broker_error("publish failed", e))?
            .await
            .map_err(|e| broker_error("publish was not confirmed", e))?;
        if confirm.is_ack() {
            tracing::debug!(event = name, "event broadcast to the exchange");
            Ok(())
        } else {
            Err(broker_error(name, "the broker rejected the message"))
        }
    }

    /// One consumer per bus instance.
    pub(crate) fn claim_consumer(&self) -> bool {
        !self.inner.consumer_started.swap(true, Ordering::SeqCst)
    }

    /// Consume the service queue until the connection dies, dispatching
    /// every delivery to the bus's local subscribers. Failed handlers are
    /// logged by the dispatch path; deliveries are acked regardless, so a
    /// poison message cannot wedge the queue.
    pub(crate) async fn consume(&self, bus: crate::events::Events) {
        let channel = match self.inner.connection.create_channel().await {
            Ok(channel) => channel,
            Err(e) => {
                tracing::error!(error = %e, "event consumer could not open a channel");
                return;
            }
        };
        let mut consumer = match channel
            .basic_consume(
                self.inner.queue.as_str().into(),
                "nebula-events".into(),
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
        {
            Ok(consumer) => consumer,
            Err(e) => {
                tracing::error!(error = %e, "event consumer could not start");
                return;
            }
        };
        tracing::info!(queue = %self.inner.queue, "event consumer started");
        while let Some(delivery) = consumer.next().await {
            let delivery = match delivery {
                Ok(delivery) => delivery,
                Err(e) => {
                    tracing::error!(error = %e, "event consumer lost the broker connection");
                    return;
                }
            };
            let name = delivery.routing_key.as_str().to_string();
            bus.dispatch_raw(&name, &delivery.data).await;
            if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                tracing::error!(event = %name, error = %e, "failed to ack an event delivery");
            }
        }
        tracing::warn!("event consumer stream ended");
    }
}
