//! Module SQL migrations against a live database: a module's `.sql`
//! files are discovered, applied in order (creating tables and indexes),
//! tracked, never re-applied, and rolled back as a unit when one fails.
//! Skips when NEBULA_TEST_DATABASE_URL is unset.
//!
//! One test drives every case: the tracking table is shared per database,
//! so splitting into parallel tests would race on its creation (a real
//! database is only ever migrated by one path at a time).

use nebula::SqlMigrator;
use nebula::config::DatabaseConfig;
use nebula::db;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::path::PathBuf;

/// A fresh temp directory to lay out a module's migration files in.
fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("nebula-sqlmig-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Count the rows the tracking table holds for a module.
async fn applied_count(db: &DatabaseConnection, module: &str) -> i64 {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT count(*) AS n FROM nebula_sql_migrations WHERE module = $1",
            [module.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "n").unwrap()
}

/// Does the named relation (table or index) exist in the public schema?
async fn exists(db: &DatabaseConnection, name: &str) -> bool {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT to_regclass($1) IS NOT NULL AS present",
            [format!("public.{name}").into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "present").unwrap()
}

async fn column_exists(db: &DatabaseConnection, table: &str, column: &str) -> bool {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_name = $1 AND column_name = $2) AS present",
            [table.into(), column.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "present").unwrap()
}

#[tokio::test]
async fn applies_orders_indexes_and_rolls_back() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let conn = db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");

    // Start clean: drop what this test creates (the tracking table may
    // not exist yet, hence the guard on the DELETE).
    conn.execute_unprepared(
        "DROP TABLE IF EXISTS demo_invoices; DROP TABLE IF EXISTS demo_broken; \
         DO $$ BEGIN IF to_regclass('public.nebula_sql_migrations') IS NOT NULL THEN \
           DELETE FROM nebula_sql_migrations WHERE module IN ('demo', 'broken'); \
         END IF; END $$;",
    )
    .await
    .expect("cleanup must work");

    // --- happy path: two ordered migrations, each with its indexes ---
    let root = temp_root();
    let module = root.join("demo");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("invoices_0001.sql"),
        "CREATE TABLE demo_invoices (\
            id UUID PRIMARY KEY, \
            customer_id UUID NOT NULL, \
            status TEXT NOT NULL, \
            issued_at TIMESTAMPTZ NOT NULL);\n\
         CREATE INDEX ix_demo_invoices_customer ON demo_invoices (customer_id);",
    )
    .unwrap();
    std::fs::write(
        module.join("invoices_0002.sql"),
        "ALTER TABLE demo_invoices ADD COLUMN total_minor BIGINT NOT NULL DEFAULT 0;\n\
         CREATE INDEX ix_demo_invoices_status ON demo_invoices (status);",
    )
    .unwrap();

    let migrator = SqlMigrator::new(&root);
    migrator.run(&conn).await.expect("first run must apply both");

    assert_eq!(applied_count(&conn, "demo").await, 2);
    assert!(exists(&conn, "ix_demo_invoices_customer").await);
    assert!(exists(&conn, "ix_demo_invoices_status").await);
    assert!(
        column_exists(&conn, "demo_invoices", "total_minor").await,
        "the second migration's column must be present"
    );

    // --- idempotent: a second run applies nothing new ---
    migrator
        .run(&conn)
        .await
        .expect("second run must be a no-op");
    assert_eq!(applied_count(&conn, "demo").await, 2);

    // --- a missing root is a silent no-op ---
    SqlMigrator::new(root.join("does-not-exist"))
        .run(&conn)
        .await
        .expect("missing root must be a no-op");

    // --- failure: a bad file rolls back whole and is not recorded ---
    let broken_root = temp_root();
    let broken = broken_root.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("t_0001.sql"),
        "CREATE TABLE demo_broken (id UUID PRIMARY KEY);\n\
         CREATE TABLE demo_broken (id UUID PRIMARY KEY);", // duplicate: fails
    )
    .unwrap();

    assert!(
        SqlMigrator::new(&broken_root).run(&conn).await.is_err(),
        "a bad file must surface an error"
    );
    assert!(
        !exists(&conn, "demo_broken").await,
        "the failed migration must roll back its table"
    );
    assert_eq!(applied_count(&conn, "broken").await, 0);

    // Tidy up.
    conn.execute_unprepared(
        "DROP TABLE IF EXISTS demo_invoices; DROP TABLE IF EXISTS demo_broken; \
         DELETE FROM nebula_sql_migrations WHERE module IN ('demo', 'broken');",
    )
    .await
    .ok();
}
