//! Layered application configuration.
//!
//! Values are resolved in order, later sources overriding earlier ones:
//!
//! 1. Built-in defaults ([`Config::default`])
//! 2. `config/{env}.yaml` — `dev.yaml`, `test.yaml` or `prod.yaml` in the
//!    application's `config/` folder, selected by `NEBULA_ENV` (default `dev`)
//! 3. `config/{env}.local.yaml` — gitignored overlay for machine-local secrets
//! 4. Environment variables prefixed `NEBULA__`, with `__` as the section
//!    separator (e.g. `NEBULA__SERVER__PORT=8080` sets `server.port`)
//!
//! Secrets (connection strings, passwords) belong in the local overlay or
//! environment variables, never in checked-in files. Validation runs at
//! load so misconfiguration fails at boot, not at first use.

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Yaml};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

pub const ENV_VAR: &str = "NEBULA_ENV";
pub const ENV_PREFIX: &str = "NEBULA__";
pub const DEFAULT_ENV: &str = "dev";

/// A string that must never appear in logs or debug output.
/// `Debug`/`Display` print `***`; call [`Secret::expose`] deliberately.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub environment: String,
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub multitenancy: MultitenancyConfig,
    pub redis: RedisConfig,
    pub rabbitmq: RabbitMqConfig,
    pub logging: LoggingConfig,
    pub auth: AuthConfig,
    pub audit: AuditConfig,
    pub jobs: JobsConfig,
    pub events: EventsConfig,
    pub files: FilesConfig,
    pub migrations: MigrationsConfig,
    pub currencies: Vec<CurrencyConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            environment: DEFAULT_ENV.into(),
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            multitenancy: MultitenancyConfig::default(),
            redis: RedisConfig::default(),
            rabbitmq: RabbitMqConfig::default(),
            logging: LoggingConfig::default(),
            auth: AuthConfig::default(),
            audit: AuditConfig::default(),
            jobs: JobsConfig::default(),
            events: EventsConfig::default(),
            files: FilesConfig::default(),
            migrations: MigrationsConfig::default(),
            currencies: Vec::new(),
        }
    }
}

/// Module SQL migrations: modules ship pure `.sql` files under
/// `{root}/<module>/`, applied on top of the framework's in-house SeaORM
/// schema and tracked so each runs once per database. Framework
/// ("system") migrations stay in code; business modules migrate here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MigrationsConfig {
    /// Directory holding per-module migration folders, relative to the
    /// working directory (or absolute).
    pub root: String,
}

impl Default for MigrationsConfig {
    fn default() -> Self {
        Self {
            root: "migrations".into(),
        }
    }
}

/// Public file storage: uploads land under
/// `{root}/{tenant-slug}/{id}/{resource}` and the whole root is served
/// at `/public`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesConfig {
    /// Directory for publicly served files, relative to the working
    /// directory (or absolute).
    pub root: String,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            root: "public".into(),
        }
    }
}

/// Background job settings. Workers connect through `redis.url`; boot
/// fails fast when jobs are enabled but Redis is unreachable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JobsConfig {
    /// Run the apalis job workers alongside the web host.
    pub enabled: bool,
    /// Concurrent jobs per worker.
    pub concurrency: usize,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            concurrency: 2,
        }
    }
}

/// Integration event settings. In-process domain events always work;
/// this section controls the RabbitMQ leg (`Events::broadcast`), which
/// connects through `rabbitmq.url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    /// Route `broadcast` events through RabbitMQ. Off: broadcasts
    /// degrade to in-process delivery (fine for a single node).
    pub distributed: bool,
    /// Topic exchange shared by every service of the deployment.
    pub exchange: String,
    /// This service's durable queue; instances share it, so each event
    /// is processed once per service. Give each service its own name.
    pub queue: String,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            distributed: false,
            exchange: "nebula.events".into(),
            queue: "nebula".into(),
        }
    }
}

/// Audit trail settings. Request bodies are never recorded — snapshots
/// come from handlers that know which safe view of an entity to log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditConfig {
    /// Record every mutating HTTP request and entity change.
    pub enabled: bool,
    /// Also record read (GET/HEAD) requests. Off by default: reads are
    /// high-volume and rarely worth a row each.
    pub include_reads: bool,
    /// System-level retention window: rows older than this are pruned.
    pub retention_days: u32,
    /// Cap for per-tenant retention overrides (six months by default).
    pub retention_max_days: u32,
    /// How often the pruning job runs.
    pub prune_interval_secs: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_reads: false,
            retention_days: 30,
            retention_max_days: 180,
            prune_interval_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Signs JWTs. Required when the auth module is used; set it in the
    /// local overlay or NEBULA__AUTH__JWT_SECRET.
    pub jwt_secret: Secret,
    pub access_token_ttl_secs: u64,
    pub refresh_token_ttl_secs: u64,
    /// Lifetime of the short-lived token that bridges password login and
    /// the two-factor step.
    pub two_factor_token_ttl_secs: u64,
    pub password_min_length: usize,
    /// Issuer shown in authenticator apps.
    pub totp_issuer: String,
    pub lockout_max_failed: i32,
    pub lockout_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: Secret::default(),
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 30 * 24 * 3600,
            two_factor_token_ttl_secs: 300,
            password_min_length: 8,
            totp_issuer: "Nebula".into(),
            lockout_max_failed: 5,
            lockout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub request_timeout_secs: u64,
    /// Browser origins allowed to call the API cross-origin (the SPA dev
    /// server, the deployed frontend). Empty disables CORS entirely.
    pub cors_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 5000,
            request_timeout_secs: 30,
            cors_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Empty means: boot without a database.
    pub url: Secret,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout_secs: u64,
    pub acquire_timeout_secs: u64,
    pub idle_timeout_secs: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MultitenancyConfig {
    /// Off: single main database (self-hosted mode). On: database per
    /// tenant with the main database as the tenant directory.
    pub enabled: bool,
    /// Request header that names the tenant.
    pub header: String,
    /// Provision a dedicated database for each new tenant (named
    /// `{slug}-{key}`) instead of sharing the main database. The
    /// `database.url` role must be allowed to `CREATE DATABASE`. Off:
    /// tenants share the main database unless created with an explicit
    /// connection string.
    pub provision_databases: bool,
}

impl Default for MultitenancyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            header: "X-Tenant".into(),
            provision_databases: false,
        }
    }
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
    Pretty,
    Json,
}

/// One entry of the application's currency table, e.g.
/// `{ code: KES, minor_units: 2 }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrencyConfig {
    pub code: String,
    pub minor_units: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(#[from] Box<figment::Error>),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(Path::new("config"))
    }

    pub fn load_from(dir: &Path) -> Result<Self, ConfigError> {
        let environment = std::env::var(ENV_VAR).unwrap_or_else(|_| DEFAULT_ENV.into());

        let config: Config = Figment::new()
            .merge(Serialized::defaults(Config {
                environment: environment.clone(),
                ..Config::default()
            }))
            .merge(Yaml::file(dir.join(format!("{environment}.yaml"))))
            .merge(Yaml::file(dir.join(format!("{environment}.local.yaml"))))
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
            return Err(ConfigError::Invalid(
                "logging.level must not be empty".into(),
            ));
        }
        if self.files.root.trim().is_empty() {
            return Err(ConfigError::Invalid("files.root must not be empty".into()));
        }
        if self.migrations.root.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "migrations.root must not be empty".into(),
            ));
        }
        if self.events.distributed {
            if self.rabbitmq.url.is_empty() {
                return Err(ConfigError::Invalid(
                    "events.distributed requires rabbitmq.url".into(),
                ));
            }
            if self.events.exchange.trim().is_empty() || self.events.queue.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "events.exchange and events.queue must not be empty".into(),
                ));
            }
        }
        crate::money::CurrencyRegistry::from_config(&self.currencies)
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(())
    }
}
