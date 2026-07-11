//! Proof of concept: two application instances booting against the same
//! fresh database at once both migrate cleanly. Without the migration
//! advisory lock this races on `CREATE TABLE IF NOT EXISTS` and one boot
//! fails with a Postgres duplicate-key error on `pg_type`. Skips when
//! NEBULA_TEST_DATABASE_URL is unset.

use nebula::config::{Config, DatabaseConfig};
use nebula::{Kernel, db};
use sea_orm::ConnectionTrait;

async fn boot(url: &str) -> nebula::Result<()> {
    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    Kernel::builder()
        .with_config(config)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .map(|_| ())
}

#[tokio::test]
async fn concurrent_boots_migrate_a_fresh_database_without_racing() {
    let Ok(main_url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    // A throwaway database so the run always starts from an unmigrated state
    // (that is when the race happens).
    let admin = db::connect(&DatabaseConfig {
        url: main_url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to create the test database");

    let fresh = format!("nebula_lock_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute_unprepared(&format!("CREATE DATABASE {fresh}"))
        .await
        .expect("must create the fresh database");

    // The main URL points at a named database; swap in the fresh one.
    let fresh_url = swap_database(&main_url, &fresh);

    // Boot two instances at the same time against the fresh database.
    let (first, second) = tokio::join!(boot(&fresh_url), boot(&fresh_url));

    let outcome = first.and(second);

    // Drop the throwaway database (force out the pools' connections) before
    // asserting, so a failure never leaks it.
    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {fresh} WITH (FORCE)"))
        .await;

    outcome.expect("both concurrent boots must migrate the fresh database");
}

/// Replace the database name (the last path segment) in a Postgres URL.
fn swap_database(url: &str, database: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database}"),
        None => format!("{url}/{database}"),
    }
}
