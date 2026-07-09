//! Layered application configuration.
//!
//! Values are resolved in order, later sources overriding earlier ones:
//!
//! 1. Built-in defaults ([`Config::default`])
//! 2. `nebula.toml` in the working directory
//! 3. `nebula.{environment}.toml` (environment from `NEBULA_ENV`, default `development`)
//! 4. Environment variables prefixed `NEBULA__`, with `__` as the section
//!    separator (e.g. `NEBULA__SERVER__PORT=8080` sets `server.port`)
//!
//! Secrets (connection strings, passwords) belong in environment variables,
//! never in checked-in files.

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// Environment variable that selects the configuration environment.
pub const ENV_VAR: &str = "NEBULA_ENV";
/// Prefix for configuration overrides from the process environment.
pub const ENV_PREFIX: &str = "NEBULA__";

/// A string that must never appear in logs or debug output (connection
/// strings, passwords, API keys). `Debug`/`Display` print `***`;
/// call [`Secret::expose`] to read the value deliberately.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Deliberately reveal the secret value.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Root configuration for a Nebula application.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Active environment name (`development`, `staging`, `production`, ...).
    pub environment: String,
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub multitenancy: MultitenancyConfig,
    pub redis: RedisConfig,
    pub rabbitmq: RabbitMqConfig,
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            environment: "development".into(),
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            multitenancy: MultitenancyConfig::default(),
            redis: RedisConfig::default(),
            rabbitmq: RabbitMqConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Requests running longer than this are aborted to protect the host.
    pub request_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 5000,
            request_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Connection string for the main (directory) database. When empty
    /// the application boots without a database (modules relying on one
    /// will say so loudly).
    pub url: Secret,
    /// Upper bound on pooled connections; protects Postgres from
    /// connection storms.
    pub max_connections: u32,
    pub min_connections: u32,
    /// Fail fast when the database is unreachable at boot.
    pub connect_timeout_secs: u64,
    /// Bound how long a request may wait for a pooled connection —
    /// prevents pool exhaustion from turning into a deadlock.
    pub acquire_timeout_secs: u64,
    /// Drop idle connections after this long.
    pub idle_timeout_secs: u64,
    /// Apply pending migrations during boot.
    pub auto_migrate: bool,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: Secret::default(),
            max_connections: 10,
            min_connections: 0,
            connect_timeout_secs: 10,
            acquire_timeout_secs: 10,
            idle_timeout_secs: 600,
            auto_migrate: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MultitenancyConfig {
    /// When disabled the application runs against the single main database
    /// (self-hosted mode). When enabled each tenant gets its own database
    /// and the main database acts as the tenant directory.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RedisConfig {
    pub url: Secret,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://127.0.0.1:6379".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RabbitMqConfig {
    pub url: Secret,
}

impl Default for RabbitMqConfig {
    fn default() -> Self {
        Self {
            url: "amqp://127.0.0.1:5672".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// A `tracing` filter directive, e.g. `info` or `nebula=debug,info`.
    pub level: String,
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Pretty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable output for development.
    Pretty,
    /// Structured JSON output for log aggregation.
    Json,
}

/// Errors raised while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] Box<figment::Error>),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    /// Load configuration from the current working directory.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(Path::new("."))
    }

    /// Load configuration rooted at `dir`, applying all layers and
    /// validating the result. Fails fast with a descriptive error so
    /// misconfiguration is caught at boot, not at first use.
    pub fn load_from(dir: &Path) -> Result<Self, ConfigError> {
        let environment = std::env::var(ENV_VAR).unwrap_or_else(|_| "development".into());

        let config: Config = Figment::new()
            .merge(Serialized::defaults(Config {
                environment: environment.clone(),
                ..Config::default()
            }))
            .merge(Toml::file(dir.join("nebula.toml")))
            .merge(Toml::file(dir.join(format!("nebula.{environment}.toml"))))
            .merge(Env::prefixed(ENV_PREFIX).split("__"))
            .extract()
            .map_err(Box::new)?;

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.environment.trim().is_empty() {
            return Err(ConfigError::Invalid("environment must not be empty".into()));
        }
        if self.server.host.trim().is_empty() {
            return Err(ConfigError::Invalid("server.host must not be empty".into()));
        }
        if self.server.port == 0 {
            return Err(ConfigError::Invalid("server.port must not be 0".into()));
        }
        if self.server.request_timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "server.request_timeout_secs must be at least 1".into(),
            ));
        }
        if self.logging.level.trim().is_empty() {
            return Err(ConfigError::Invalid("logging.level must not be empty".into()));
        }
        Ok(())
    }
}
