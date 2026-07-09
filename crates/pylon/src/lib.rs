//! Pylon — a DDD application framework for building ERPs in Rust.
//!
//! Inspired by ASP.NET Boilerplate: applications are composed from modules,
//! bootstrapped by a kernel, and configured rather than hardcoded.

pub mod config;
pub mod error;
pub mod logging;

pub use config::Config;
pub use error::{Error, Result};
