//! Cross-context contracts ("ports"): framework-owned types that let one
//! bounded context react to another without either importing the other.
//!
//! Apps never depend on each other — they all depend on the framework. A
//! port is therefore a plain contract type living here: the publishing
//! side fills it from its own domain, the subscribing side interprets it
//! in its own, and the only shared vocabulary is this module. Ports ride
//! the [`events`](crate::events) bus, so they inherit its delivery
//! semantics (in-process publish, or RabbitMQ broadcast when distributed
//! events are on).

pub mod gl;
