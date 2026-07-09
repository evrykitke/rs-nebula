//! Nebula — a DDD application framework for building ERPs in Rust.
//!
//! Inspired by ASP.NET Boilerplate: applications are composed from modules,
//! bootstrapped by a kernel, and configured rather than hardcoded.

pub mod config;
pub mod db;
pub mod error;
pub mod kernel;
pub mod logging;
pub mod module;
pub mod repository;
pub mod time;
mod web;

/// Exact decimal arithmetic for money and quantities — never `f64`,
/// which cannot represent amounts like 0.1 exactly and accumulates
/// rounding errors.
pub use rust_decimal::Decimal;

/// Re-exported so applications use the same SeaORM the framework links.
pub use sea_orm;
pub use sea_orm_migration;

pub use config::Config;
pub use error::{Error, Result};
pub use kernel::{App, Kernel};
pub use module::{Module, ModuleContext};
pub use repository::Repository;
pub use time::{Clock, SystemClock};
