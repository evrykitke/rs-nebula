//! Plain-SQL migrations for application modules.
//!
//! The framework's own ("system", in-house) schema is owned by the
//! SeaORM migrator in [`crate::migrations`] and stays in code. Business
//! modules instead ship pure `.sql` files under `migrations/<module>/`,
//! discovered from disk, applied in filename order and tracked in
//! `nebula_sql_migrations` so each runs exactly once per database — on
//! the main database and on every tenant database, wherever the
//! framework migrations run.
//!
//! Convention:
//! - one migration per file, e.g. `migrations/sales/invoices_0001.sql`
//! - name files so lexical order is apply order (zero-pad the number)
//! - create the indexes a table needs in the same file: a foreign key or
//!   a column filtered/joined on should be indexed, so reads never fall
//!   back to a sequential scan
//!
//! A file runs inside a transaction, so statements that cannot
//! (`CREATE INDEX CONCURRENTLY`) are not supported here — use a plain
//! `CREATE INDEX`.

use crate::config::MigrationsConfig;
use crate::error::{Error, Result};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, TransactionTrait};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Applies the module SQL migrations discovered under a root directory.
/// Cheap to construct; holds only the root path.
pub struct SqlMigrator {
    root: PathBuf,
}

impl SqlMigrator {
    pub fn from_config(config: &MigrationsConfig) -> Self {
        Self::new(&config.root)
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Apply every not-yet-applied module migration to `db`. A missing
    /// root is a no-op — an application may ship no module SQL at all.
    pub async fn run(&self, db: &DatabaseConnection) -> Result<()> {
        if !self.root.is_dir() {
            return Ok(());
        }
        ensure_tracking_table(db).await?;

        for module_dir in sorted_dirs(&self.root)? {
            let module = module_dir
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| Error::internal(format!("invalid module directory {module_dir:?}")))?
                .to_string();
            let applied = applied_names(db, &module).await?;
            for file in sorted_sql_files(&module_dir)? {
                let name = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if applied.contains(&name) {
                    continue;
                }
                let sql = std::fs::read_to_string(&file)
                    .map_err(|e| Error::internal(format!("failed to read {file:?}: {e}")))?;
                apply_file(db, &module, &name, &sql).await?;
                tracing::info!(module = %module, migration = %name, "applied SQL migration");
            }
        }
        Ok(())
    }
}

/// The tracking table lives alongside the framework's own migration
/// table but is separate: framework schema is SeaORM's concern, module
/// SQL is this migrator's.
async fn ensure_tracking_table(db: &DatabaseConnection) -> Result<()> {
    db.execute_unprepared(
        "CREATE TABLE IF NOT EXISTS nebula_sql_migrations (\
            module TEXT NOT NULL, \
            name TEXT NOT NULL, \
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            PRIMARY KEY (module, name))",
    )
    .await
    .map_err(Error::from)?;
    Ok(())
}

async fn applied_names(db: &DatabaseConnection, module: &str) -> Result<HashSet<String>> {
    let backend = db.get_database_backend();
    let rows = db
        .query_all(Statement::from_sql_and_values(
            backend,
            "SELECT name FROM nebula_sql_migrations WHERE module = $1",
            [module.to_string().into()],
        ))
        .await?;
    let mut names = HashSet::with_capacity(rows.len());
    for row in rows {
        names.insert(row.try_get::<String>("", "name")?);
    }
    Ok(names)
}

/// Run one file's statements and record it, atomically: either the whole
/// migration lands and is marked applied, or nothing does.
async fn apply_file(db: &DatabaseConnection, module: &str, name: &str, sql: &str) -> Result<()> {
    let backend = db.get_database_backend();
    let txn = db.begin().await?;
    if let Err(e) = txn.execute_unprepared(sql).await {
        let _ = txn.rollback().await;
        return Err(Error::internal(format!(
            "SQL migration {module}/{name} failed: {e}"
        )));
    }
    txn.execute(Statement::from_sql_and_values(
        backend,
        "INSERT INTO nebula_sql_migrations (module, name) VALUES ($1, $2)",
        [module.to_string().into(), name.to_string().into()],
    ))
    .await?;
    txn.commit().await?;
    Ok(())
}

fn sorted_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|e| Error::internal(format!("failed to read migrations root {root:?}: {e}")))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn sorted_sql_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| Error::internal(format!("failed to read migration directory {dir:?}: {e}")))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();
    Ok(files)
}
