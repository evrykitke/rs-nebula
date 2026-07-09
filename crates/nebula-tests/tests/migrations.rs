//! Proof of concept: the kernel applies registered migrations at boot
//! when `database.auto_migrate` is enabled.

use nebula::config::{Config, DatabaseConfig};
use nebula::{Kernel, db};
use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

// --- A minimal migrator, as an application would define it ---

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(CreatePocNotes)]
    }
}

struct CreatePocNotes;

impl MigrationName for CreatePocNotes {
    fn name(&self) -> &str {
        "m20260709_000001_create_poc_notes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreatePocNotes {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(PocNotes::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PocNotes::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(PocNotes::Title).string().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PocNotes::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum PocNotes {
    Table,
    Id,
    Title,
}

// --- The proof ---

fn test_db_url() -> Option<String> {
    let url = std::env::var("NEBULA_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
    }
    url
}

#[tokio::test]
async fn kernel_applies_migrations_at_boot() {
    let Some(url) = test_db_url() else { return };

    // Clean slate so the test is repeatable.
    let cleanup = db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect for cleanup");
    cleanup
        .execute_unprepared(
            "DROP TABLE IF EXISTS poc_notes; \
             DELETE FROM seaql_migrations WHERE version LIKE '%poc_notes%';",
        )
        .await
        .ok();

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };

    let app = Kernel::builder()
        .with_config(config)
        .with_migrations::<Migrator>()
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot with migrations must succeed");

    let db = app.database().expect("database must be connected");
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT to_regclass('public.poc_notes')::text AS tbl",
        ))
        .await
        .expect("query must run")
        .expect("query must return a row");
    let table: Option<String> = row.try_get("", "tbl").expect("column must exist");
    assert_eq!(
        table.as_deref(),
        Some("poc_notes"),
        "migration must have created the table"
    );

    // The schema is actually usable.
    let inserted = db
        .execute_unprepared("INSERT INTO poc_notes (title) VALUES ('proof of concept')")
        .await
        .expect("insert must work");
    assert_eq!(inserted.rows_affected(), 1);
}
