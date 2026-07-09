//! Logging bootstrap built on `tracing`.
//!
//! The filter comes from `logging.level` in configuration; a `RUST_LOG`
//! environment variable, when present, takes precedence so developers can
//! turn diagnostics up without touching config files.

use crate::config::{LogFormat, LoggingConfig};
use tracing_subscriber::EnvFilter;

/// Errors raised while initializing logging.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("invalid logging filter {directive:?}: {source}")]
    InvalidFilter {
        directive: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    #[error("logging already initialized")]
    AlreadyInitialized,
}

/// Install the global tracing subscriber according to configuration.
/// Call once at boot (the kernel does this); a second call fails.
pub fn init(config: &LoggingConfig) -> Result<(), LoggingError> {
    let directive = std::env::var("RUST_LOG").unwrap_or_else(|_| config.level.clone());
    let filter =
        EnvFilter::try_new(&directive).map_err(|source| LoggingError::InvalidFilter {
            directive: directive.clone(),
            source,
        })?;

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    let installed = match config.format {
        LogFormat::Pretty => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };

    installed.map_err(|_| LoggingError::AlreadyInitialized)
}
