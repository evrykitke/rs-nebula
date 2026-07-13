//! Multitenancy: each tenant may have its own database, with the main
//! database acting as the tenant directory. Fully toggleable — with
//! `multitenancy.enabled: false` the application runs single-tenant
//! against the main database (self-hosted mode).
//!
//! Pieces:
//! - [`tenant`] — the directory entity (`tenants` table in the main db;
//!   schema in [`crate::migrations`])
//! - [`TenantManager`] — directory lookups, tenant creation, and a lazy
//!   cache of per-tenant connection pools
//! - request resolution middleware and extractors live in `middleware`
//!   (wired by the web layer when multitenancy is enabled)

pub mod middleware;
pub mod tenant;

use crate::config::{DatabaseConfig, MultitenancyConfig};
use crate::db;
use crate::error::{Error, Result};
use crate::kernel::Migrations;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, Set,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use uuid::Uuid;

/// The resolved tenant of the current request, inserted into request
/// extensions by the resolution middleware.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TenantRef {
    pub id: Uuid,
    pub name: String,
}

/// Published when a tenant is registered — the hook for provisioning
/// reactions (workspace resources, welcome flows) in other contexts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TenantCreated {
    pub tenant_id: Uuid,
    pub name: String,
    pub display_name: String,
}

impl crate::events::Event for TenantCreated {
    const NAME: &'static str = "tenancy.tenant_created";
}

/// Published when a company's currency is set or changed on its profile.
/// A currency is only half a decision here: modules that denominate their
/// own rows (a ledger's chart of accounts) hold a copy of it and have to
/// follow. Onboarding leans on this — the company is created first and
/// picks its currency afterwards.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TenantCurrencyChanged {
    pub tenant_id: Uuid,
    /// The code now on the profile; `None` cleared it.
    pub currency: Option<String>,
}

impl crate::events::Event for TenantCurrencyChanged {
    const NAME: &'static str = "tenancy.tenant_currency_changed";
}

pub struct NewTenant {
    pub name: String,
    pub display_name: String,
    /// `None` shares the main database.
    pub connection_string: Option<String>,
    /// Currency code the company operates in, validated by the caller
    /// against the currency table.
    pub default_currency: Option<String>,
}

/// Editable company-profile fields (see `PUT /auth/tenant/profile`).
pub struct CompanyProfile {
    pub display_name: String,
    pub default_currency: Option<String>,
    pub tax_pin: Option<String>,
    pub vat_number: Option<String>,
    pub address: Option<String>,
    pub email: Option<String>,
    pub website: Option<String>,
    pub phone: Option<String>,
}

/// Directory lookups and per-tenant connection pooling. One instance is
/// created by the kernel when multitenancy is enabled and shared
/// application-wide.
pub struct TenantManager {
    main: DatabaseConnection,
    db_config: DatabaseConfig,
    config: MultitenancyConfig,
    /// The full migration set (framework + application + module SQL), run
    /// against a freshly provisioned tenant database.
    migrations: Migrations,
    pools: RwLock<HashMap<Uuid, DatabaseConnection>>,
    /// Short-TTL cache of `name → row` for request resolution, so the
    /// per-request directory lookup does not hit the main database every
    /// time. Writes through this manager invalidate immediately; other
    /// instances converge within `multitenancy.directory_cache_secs`.
    directory: RwLock<HashMap<String, (tenant::Model, Instant)>>,
}

impl TenantManager {
    pub(crate) fn new(
        main: DatabaseConnection,
        db_config: DatabaseConfig,
        config: MultitenancyConfig,
        migrations: Migrations,
    ) -> Self {
        Self {
            main,
            db_config,
            config,
            migrations,
            pools: RwLock::new(HashMap::new()),
            directory: RwLock::new(HashMap::new()),
        }
    }

    pub fn header_name(&self) -> &str {
        &self.config.header
    }

    pub fn main_db(&self) -> &DatabaseConnection {
        &self.main
    }

    pub async fn find_by_name(&self, name: &str) -> Result<Option<tenant::Model>> {
        tenant::Entity::find()
            .filter(tenant::Column::Name.eq(name))
            .one(&self.main)
            .await
            .map_err(Error::from)
    }

    /// [`find_by_name`] through the short-TTL directory cache — the
    /// per-request resolution path. Only positive hits are cached (an
    /// unknown name stays a directory query, so a freshly created tenant
    /// is visible immediately).
    ///
    /// [`find_by_name`]: TenantManager::find_by_name
    pub async fn resolve(&self, name: &str) -> Result<Option<tenant::Model>> {
        let ttl = Duration::from_secs(self.config.directory_cache_secs);
        if !ttl.is_zero()
            && let Some((tenant, at)) = self.directory.read().await.get(name)
            && at.elapsed() < ttl
        {
            return Ok(Some(tenant.clone()));
        }
        let found = self.find_by_name(name).await?;
        if !ttl.is_zero()
            && let Some(tenant) = &found
        {
            self.directory
                .write()
                .await
                .insert(name.to_string(), (tenant.clone(), Instant::now()));
        }
        Ok(found)
    }

    /// Drop a tenant's cached directory entry — called after every write
    /// through this manager so this instance never serves a stale row.
    async fn invalidate(&self, name: &str) {
        self.directory.write().await.remove(name);
    }

    /// Drop a tenant's cached connection pool. Call after changing where
    /// a tenant's data lives (its `connection_string`) or deactivating
    /// it; the next request builds a fresh pool from the directory row.
    pub async fn evict_pool(&self, tenant_id: Uuid) {
        self.pools.write().await.remove(&tenant_id);
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<tenant::Model>> {
        tenant::Entity::find_by_id(id)
            .one(&self.main)
            .await
            .map_err(Error::from)
    }

    /// Activate or deactivate a tenant. A deactivated tenant resolves
    /// like an unknown one (404) and its cached connection pool is
    /// evicted, so its database is released; the directory row and data
    /// stay put for reactivation.
    pub async fn set_active(&self, id: Uuid, active: bool) -> Result<tenant::Model> {
        let tenant = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("tenant {id}")))?;
        let mut model: tenant::ActiveModel = tenant.into();
        model.is_active = Set(active);
        let updated = self.saved(model.update(&self.main).await?).await?;
        if !active {
            self.evict_pool(id).await;
        }
        Ok(updated)
    }

    /// Compensation for a registration that failed after the tenant was
    /// created: delete the directory row (and its cache entries), and —
    /// when the caller knows the database was provisioned by us, not
    /// brought by the tenant — drop that database too. Best-effort by
    /// design: it already runs on an error path.
    pub async fn remove(&self, tenant: &tenant::Model, drop_provisioned_database: bool) {
        self.invalidate(&tenant.name).await;
        self.evict_pool(tenant.id).await;
        if let Err(e) = tenant::Entity::delete_by_id(tenant.id).exec(&self.main).await {
            tracing::error!(tenant = %tenant.name, error = %e,
                "failed to delete a stranded tenant row during compensation");
            return;
        }
        if drop_provisioned_database
            && let Some(name) = tenant
                .connection_string
                .as_deref()
                .and_then(database_name_of)
        {
            self.drop_database(name).await;
        }
    }

    /// Company-wide two-factor policy: when on, every user of the tenant
    /// must set up an authenticator app before they can sign in.
    pub async fn set_require_two_factor(&self, id: Uuid, required: bool) -> Result<tenant::Model> {
        let tenant = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("tenant {id}")))?;
        let mut active: tenant::ActiveModel = tenant.into();
        active.require_two_factor = Set(required);
        self.saved(active.update(&self.main).await?).await
    }

    /// Post-write bookkeeping shared by every tenant mutation: drop the
    /// stale directory-cache entry, then hand the row back.
    async fn saved(&self, updated: tenant::Model) -> Result<tenant::Model> {
        self.invalidate(&updated.name).await;
        Ok(updated)
    }

    /// Tenant override of the audit retention window; `None` reverts to
    /// the system default. The cap is enforced by the caller, which
    /// knows the configured maximum.
    pub async fn set_audit_retention(&self, id: Uuid, days: Option<i32>) -> Result<tenant::Model> {
        let tenant = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("tenant {id}")))?;
        let mut active: tenant::ActiveModel = tenant.into();
        active.audit_retention_days = Set(days);
        self.saved(active.update(&self.main).await?).await
    }

    /// Replace the editable company-profile fields. Currency validity is
    /// the caller's concern — it knows the currency table.
    pub async fn update_profile(
        &self,
        id: Uuid,
        profile: CompanyProfile,
    ) -> Result<tenant::Model> {
        let tenant = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("tenant {id}")))?;
        let mut active: tenant::ActiveModel = tenant.into();
        active.display_name = Set(profile.display_name);
        active.default_currency = Set(profile.default_currency);
        active.tax_pin = Set(profile.tax_pin);
        active.vat_number = Set(profile.vat_number);
        active.address = Set(profile.address);
        active.email = Set(profile.email);
        active.website = Set(profile.website);
        active.phone = Set(profile.phone);
        self.saved(active.update(&self.main).await?).await
    }

    /// Record where the uploaded company logo lives, relative to the
    /// public file root.
    pub async fn set_logo_path(&self, id: Uuid, path: Option<String>) -> Result<tenant::Model> {
        let tenant = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("tenant {id}")))?;
        let mut active: tenant::ActiveModel = tenant.into();
        active.logo_path = Set(path);
        self.saved(active.update(&self.main).await?).await
    }

    pub async fn find_all(&self) -> Result<Vec<tenant::Model>> {
        tenant::Entity::find()
            .all(&self.main)
            .await
            .map_err(Error::from)
    }

    pub async fn create(&self, new: NewTenant) -> Result<tenant::Model> {
        validate_name(&new.name)?;
        if self.find_by_name(&new.name).await?.is_some() {
            return Err(Error::Conflict(format!(
                "tenant {:?} already exists",
                new.name
            )));
        }
        // Registration is unauthenticated and a tenant is expensive (a
        // whole database), so deployments can cap how many exist.
        if self.config.max_tenants > 0 {
            let count = tenant::Entity::find().count(&self.main).await?;
            if count >= self.config.max_tenants as u64 {
                return Err(Error::Validation(format!(
                    "this deployment is limited to {} tenants (multitenancy.max_tenants)",
                    self.config.max_tenants
                )));
            }
        }
        // A tenant without its own database transparently shares every
        // business table with the other shared tenants — module rows
        // carry no tenant column. That must be a deliberate deployment
        // decision, never a config accident.
        if new.connection_string.is_none()
            && !self.config.provision_databases
            && !self.config.allow_shared_database
        {
            return Err(Error::Validation(
                "refusing to create a tenant on the shared main database: business \
                 modules have no per-row tenant isolation there. Enable \
                 multitenancy.provision_databases (recommended), pass an explicit \
                 connection_string, or opt in with multitenancy.allow_shared_database"
                    .into(),
            ));
        }

        // Provision a dedicated database when configured, unless the
        // caller already named one to use. The database is created and
        // fully migrated before the tenant row exists, so a tenant is
        // only ever recorded once its store is ready.
        let mut provisioned = None;
        let connection_string = match new.connection_string {
            Some(url) => Some(url),
            None if self.config.provision_databases => {
                let db = self.provision_database(&new.name).await?;
                let url = db.url.clone();
                provisioned = Some(db);
                Some(url)
            }
            None => None,
        };

        let inserted = tenant::ActiveModel {
            id: Set(Uuid::new_v4()),
            name: Set(new.name),
            display_name: Set(new.display_name),
            connection_string: Set(connection_string),
            is_active: Set(true),
            require_two_factor: Set(false),
            default_currency: Set(new.default_currency),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&self.main)
        .await
        .map_err(Error::from);

        // If recording the tenant fails after we cut a database for it,
        // drop the orphan rather than leave it stranded on the server.
        if inserted.is_err()
            && let Some(db) = provisioned
        {
            self.drop_database(&db.name).await;
        }
        inserted
    }

    /// Cut a fresh database for a tenant slug — `CREATE DATABASE`, then
    /// run the framework and application migrations against it — and
    /// return its name and connection string. On any failure after the
    /// database exists it is dropped, so a half-built database never
    /// lingers.
    async fn provision_database(&self, slug: &str) -> Result<ProvisionedDatabase> {
        let name = new_database_name(slug);
        // Identifier is derived from a validated slug plus a random key,
        // but quote it so a dash (or a future name shape) is always safe.
        self.main
            .execute_unprepared(&format!("CREATE DATABASE \"{name}\""))
            .await
            .map_err(|e| Error::internal(format!("failed to create database {name:?}: {e}")))?;

        let url = tenant_database_url(self.db_config.url.expose(), &name)?;
        match self.migrate_fresh(&url).await {
            Ok(()) => {
                tracing::info!(tenant = %slug, database = %name, "provisioned tenant database");
                Ok(ProvisionedDatabase { name, url })
            }
            Err(e) => {
                self.drop_database(&name).await;
                Err(e)
            }
        }
    }

    /// Bring a newly created database up to the current schema: the full
    /// migration set (framework, application, then module SQL).
    async fn migrate_fresh(&self, url: &str) -> Result<()> {
        let db = db::connect(&DatabaseConfig {
            url: url.into(),
            ..self.db_config.clone()
        })
        .await?;
        self.migrations.apply(&db).await
    }

    /// Best-effort drop used to compensate a failed provision — a failure
    /// here is logged, never surfaced, since it is already cleanup for an
    /// earlier error. `FORCE` evicts the pool's lingering connections.
    async fn drop_database(&self, name: &str) {
        if let Err(e) = self
            .main
            .execute_unprepared(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await
        {
            tracing::error!(database = %name, error = %e,
                "failed to drop tenant database during compensation");
        }
    }

    /// The connection to use for this tenant: its own pool (created
    /// lazily, cached by tenant id) or the shared main database. Tenant
    /// pools are sized by `multitenancy.tenant_max_connections`, not the
    /// main pool's `database.max_connections` — with many tenants the
    /// totals multiply.
    pub async fn connection_for(&self, tenant: &tenant::Model) -> Result<DatabaseConnection> {
        let Some(url) = tenant
            .connection_string
            .as_deref()
            .filter(|s| !s.is_empty())
        else {
            return Ok(self.main.clone());
        };

        if let Some(db) = self.pools.read().await.get(&tenant.id) {
            return Ok(db.clone());
        }

        let db = db::connect(&DatabaseConfig {
            url: url.into(),
            max_connections: self.config.tenant_max_connections.max(1),
            min_connections: 0,
            ..self.db_config.clone()
        })
        .await?;
        // Two first requests may race here; the loser's fresh pool is
        // dropped (closing it) and everyone converges on one.
        Ok(self
            .pools
            .write()
            .await
            .entry(tenant.id)
            .or_insert(db)
            .clone())
    }
}

/// A database cut for a tenant: its Postgres name and the connection
/// string recorded on the tenant row.
struct ProvisionedDatabase {
    name: String,
    url: String,
}

/// A tenant database name: the slug, a dash, and a short random key
/// (`acme-5jy78k`) so re-using a slug can never clash and the name stays
/// within Postgres's 63-byte identifier limit.
fn new_database_name(slug: &str) -> String {
    const KEY_LEN: usize = 6;
    // Reserve room for the dash and key; the slug is ASCII (validated),
    // so truncating on a byte boundary is safe.
    const MAX_SLUG: usize = 63 - 1 - KEY_LEN;
    let slug = if slug.len() > MAX_SLUG {
        &slug[..MAX_SLUG]
    } else {
        slug
    };
    format!("{slug}-{}", random_key(KEY_LEN))
}

fn random_key(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Derive a tenant's connection string from the main database URL by
/// swapping the database name, keeping the scheme, credentials, host and
/// any query parameters. Postgres URLs carry the database as the single
/// path segment: `scheme://authority/dbname[?query]`.
fn tenant_database_url(base: &str, db_name: &str) -> Result<String> {
    let authority_start = base
        .find("://")
        .map(|i| i + 3)
        .ok_or_else(|| Error::internal("database.url has no scheme"))?;
    let path_start = base[authority_start..]
        .find('/')
        .map(|i| authority_start + i)
        .ok_or_else(|| Error::internal("database.url names no database to swap"))?;
    let query_start = base[path_start..].find('?').map(|i| path_start + i);

    let mut url = String::with_capacity(base.len() + db_name.len());
    url.push_str(&base[..=path_start]); // scheme://authority/
    url.push_str(db_name);
    if let Some(q) = query_start {
        url.push_str(&base[q..]);
    }
    Ok(url)
}

/// The database a connection string points at — the inverse of
/// [`tenant_database_url`], for dropping a provisioned database during
/// compensation. `scheme://authority/dbname[?query]`.
fn database_name_of(url: &str) -> Option<&str> {
    let authority_start = url.find("://")? + 3;
    let path_start = url[authority_start..].find('/')? + authority_start + 1;
    let name = match url[path_start..].find('?') {
        Some(q) => &url[path_start..path_start + q],
        None => &url[path_start..],
    };
    (!name.is_empty()).then_some(name)
}

/// Slugs a tenant may not claim. The slug doubles as a public URL path
/// (`/public/{slug}/…`), a cache namespace and a database-name prefix,
/// so names the framework (or a web deployment) uses for itself would
/// collide — `reports` is the host's own artifact container, `global`
/// the host cache scope.
const RESERVED_NAMES: &[&str] = &[
    "admin", "api", "app", "auth", "global", "health", "host", "nebula", "public", "report",
    "reports", "swagger-ui", "system", "tenant", "tenants", "www",
];

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(Error::Validation(format!(
            "tenant name must be 1-64 lowercase letters, digits or dashes, got {name:?}"
        )));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(Error::Validation(format!(
            "tenant name {name:?} is reserved"
        )));
    }
    Ok(())
}
