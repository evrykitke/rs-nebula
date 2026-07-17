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
    pub cache: CacheConfig,
    pub rabbitmq: RabbitMqConfig,
    pub logging: LoggingConfig,
    pub auth: AuthConfig,
    pub security: SecurityConfig,
    pub audit: AuditConfig,
    pub jobs: JobsConfig,
    pub events: EventsConfig,
    pub files: FilesConfig,
    pub mail: MailConfig,
    pub migrations: MigrationsConfig,
    pub reporting: ReportingConfig,
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
            cache: CacheConfig::default(),
            rabbitmq: RabbitMqConfig::default(),
            logging: LoggingConfig::default(),
            auth: AuthConfig::default(),
            security: SecurityConfig::default(),
            audit: AuditConfig::default(),
            jobs: JobsConfig::default(),
            events: EventsConfig::default(),
            files: FilesConfig::default(),
            mail: MailConfig::default(),
            migrations: MigrationsConfig::default(),
            reporting: ReportingConfig::default(),
            currencies: Vec::new(),
        }
    }
}

/// Reporting engine settings. Background renders store their artifact on
/// disk (under `files.private_root`); the retention sweep keeps that
/// from piling up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportingConfig {
    /// Days a background report artifact (and its job row) is kept before
    /// the pruner deletes it. `0` disables pruning.
    pub artifact_retention_days: u32,
    /// How often the artifact pruning sweep runs.
    pub prune_interval_secs: u64,
}

impl Default for ReportingConfig {
    fn default() -> Self {
        Self {
            artifact_retention_days: 7,
            prune_interval_secs: 3600,
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

/// File storage. Public uploads land under
/// `{root}/{tenant-slug}/{id}/{resource}` and the whole root is served
/// at `/public`. Private files (report artifacts, anything that must
/// only leave through a permission-checked handler) live under
/// `private_root`, which is **never** served directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesConfig {
    /// Directory for publicly served files, relative to the working
    /// directory (or absolute).
    pub root: String,
    /// Directory for private files, relative to the working directory
    /// (or absolute). Must differ from `root` — anything under `root`
    /// is reachable without authentication.
    pub private_root: String,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            root: "public".into(),
            private_root: "private".into(),
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
    /// Issuer shown in authenticator apps.
    pub totp_issuer: String,

    // The deployment's password policy. A tenant admin may tighten any of
    // these from company settings; a tenant that has not chosen inherits
    // the value here. See [`crate::auth::policy::PasswordPolicy`].
    pub password_min_length: usize,
    pub password_require_uppercase: bool,
    pub password_require_lowercase: bool,
    pub password_require_digit: bool,
    pub password_require_symbol: bool,
    /// Force a change this many days after the last one. `0` never expires.
    pub password_expiry_days: u32,
    /// Refuse a password matching any of the last N. `0` allows reuse.
    pub password_history_count: u32,
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
            totp_issuer: "Nebula".into(),
            password_min_length: 8,
            password_require_uppercase: false,
            password_require_lowercase: false,
            password_require_digit: false,
            password_require_symbol: false,
            password_expiry_days: 0,
            password_history_count: 0,
            lockout_max_failed: 5,
            lockout_secs: 300,
        }
    }
}

/// Secrets the deployment holds for itself, as opposed to the ones it
/// checks (passwords) or issues (JWTs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Encrypts secrets the system must read back — currently tenant SMTP
    /// passwords. Required only once a tenant configures mail. Changing it
    /// strands anything already encrypted: the setting reads as unset and
    /// an admin re-enters it.
    pub encryption_key: Secret,
}

/// Outbound mail. Credentials are *not* here: each tenant configures its
/// own SMTP server in company settings, so one deployment can send as many
/// different companies. This section is the machinery around that.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    /// How long to wait on the SMTP server before giving up. A hung relay
    /// must not hang the request that triggered the send.
    pub timeout_secs: u64,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self { timeout_secs: 10 }
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
    /// Statements slower than this many milliseconds are logged at `warn`
    /// even when database debug tracing is off. 0 disables the check.
    pub slow_statement_millis: u64,
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
            slow_statement_millis: 1000,
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
    /// `database.url` role must be allowed to `CREATE DATABASE`.
    ///
    /// On by default, and it should stay on: a business module's rows
    /// carry no tenant column — a tenant's isolation *is* the database it
    /// is handed. Turning this off puts every tenant in the main
    /// database, where they transparently share one set of module tables
    /// (one chart of accounts, one journal). Only turn it off for a
    /// single-tenant deployment, or when every tenant is created with an
    /// explicit `connection_string`.
    pub provision_databases: bool,
    /// Permit creating tenants that share the main database (no
    /// provisioned database, no explicit `connection_string`). Off by
    /// default because shared tenants transparently see each other's
    /// business data — module tables carry no tenant column. Creating a
    /// shared tenant without this explicit opt-in is refused.
    pub allow_shared_database: bool,
    /// Cap on the number of tenants this deployment will register; `0`
    /// means unlimited. Registration is an unauthenticated endpoint that
    /// provisions a whole database, so production deployments should set
    /// a ceiling (and rate-limit `/auth/register` at the proxy).
    pub max_tenants: u32,
    /// Pool size for each tenant's own database connection pool. Kept
    /// deliberately smaller than `database.max_connections`: with N
    /// tenants the server may hold up to `N × tenant_max_connections`
    /// connections.
    pub tenant_max_connections: u32,
    /// How long (seconds) a resolved tenant row may be served from the
    /// in-process directory cache before the main database is asked
    /// again; `0` disables caching. Writes through the tenant manager
    /// invalidate immediately; on other instances staleness is bounded
    /// by this TTL (deactivating a tenant takes effect within it).
    pub directory_cache_secs: u64,
}

impl Default for MultitenancyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            header: "X-Tenant".into(),
            provision_databases: true,
            allow_shared_database: false,
            max_tenants: 0,
            tenant_max_connections: 5,
            directory_cache_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RedisConfig {
    pub url: Secret,
}

/// Redis-backed caching. Connects through `redis.url` (shared with the
/// job queue). When off, the cache is a transparent no-op — reads always
/// miss and writes are dropped — so modules can use it unconditionally.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Cache through Redis. Off: every operation degrades to a no-op.
    /// On but Redis unreachable at boot fails fast, like the job queue.
    pub enabled: bool,
    /// Key namespace shared by every entry, so several applications (or
    /// environments) can share one Redis without colliding.
    pub prefix: String,
    /// Expiry applied by the convenience methods that don't take one
    /// (`cached`, `set_default`); explicit `get_or_set`/`set` override it.
    pub default_ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prefix: "nebula".into(),
            default_ttl_secs: 300,
        }
    }
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
    /// HTTP request tracing, overriding `level` for that area: `debug`
    /// adds a request-start line to the per-request completion line,
    /// `off` silences request tracing. Empty means `level` applies.
    pub http: String,
    /// Database tracing, overriding `level` for that area: `debug` logs
    /// every SQL statement with its timing. Empty means `level` applies.
    pub database: String,
    pub format: LogFormat,
    /// Also append logs to this file (the console output is unaffected).
    /// Empty means console only. Relative paths are resolved against the
    /// process working directory, e.g. `var/dev.log`.
    pub file: String,
    /// Roll the log file over once it reaches this many bytes: the current
    /// file is moved aside to `<file>.1` (replacing any previous roll) and
    /// a fresh file is started, so a single file never grows without bound.
    pub max_file_bytes: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            http: String::new(),
            database: String::new(),
            format: LogFormat::Pretty,
            file: String::new(),
            max_file_bytes: 5 * 1024 * 1024,
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
        if self.files.private_root.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "files.private_root must not be empty".into(),
            ));
        }
        if self.files.private_root.trim() == self.files.root.trim() {
            return Err(ConfigError::Invalid(
                "files.private_root must differ from files.root — everything under \
                 files.root is served without authentication at /public"
                    .into(),
            ));
        }
        if self.multitenancy.enabled && self.multitenancy.tenant_max_connections == 0 {
            return Err(ConfigError::Invalid(
                "multitenancy.tenant_max_connections must be at least 1".into(),
            ));
        }
        if self.migrations.root.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "migrations.root must not be empty".into(),
            ));
        }
        if self.cache.enabled {
            if self.redis.url.is_empty() {
                return Err(ConfigError::Invalid(
                    "cache.enabled requires redis.url".into(),
                ));
            }
            if self.cache.prefix.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "cache.prefix must not be empty".into(),
                ));
            }
            if self.cache.default_ttl_secs == 0 {
                return Err(ConfigError::Invalid(
                    "cache.default_ttl_secs must be at least 1".into(),
                ));
            }
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
