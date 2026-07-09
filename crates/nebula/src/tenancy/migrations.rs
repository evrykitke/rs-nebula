//! Framework-owned migrations for the tenant directory. They track in
//! their own `nebula_migrations` table so they never collide with the
//! application's migrator.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::DbErr;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(CreateTenants)]
    }

    fn migration_table_name() -> sea_orm::DynIden {
        Alias::new("nebula_migrations").into_iden()
    }
}

struct CreateTenants;

impl MigrationName for CreateTenants {
    fn name(&self) -> &str {
        "m20260709_000001_create_tenants"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateTenants {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Tenants::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Tenants::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Tenants::Name).string_len(64).not_null().unique_key())
                    .col(ColumnDef::new(Tenants::DisplayName).string().not_null())
                    .col(ColumnDef::new(Tenants::ConnectionString).string().null())
                    .col(
                        ColumnDef::new(Tenants::IsActive)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(
                        ColumnDef::new(Tenants::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Tenants::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Tenants {
    Table,
    Id,
    Name,
    DisplayName,
    ConnectionString,
    IsActive,
    CreatedAt,
}
