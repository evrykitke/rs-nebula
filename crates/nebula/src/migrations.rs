//! Framework-owned migrations: the tenant directory and the user store.
//! They track in their own `nebula_migrations` table so they never
//! collide with the application's migrator, and run on the main database
//! and every tenant database with its own connection string.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::DbErr;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(CreateTenants),
            Box::new(CreateUsers),
            Box::new(CreateRefreshTokens),
            Box::new(CreateRolesAndPermissions),
            Box::new(CreateAuditLogs),
            Box::new(AddAuditLogMessage),
            Box::new(AddTenantAuditRetention),
            Box::new(CreateUserDirectory),
            Box::new(CreateCurrencies),
            Box::new(AddTenantCompanyProfile),
        ]
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
                    .col(ColumnDef::new(Tenants::Id).uuid().not_null().primary_key())
                    .col(
                        ColumnDef::new(Tenants::Name)
                            .string_len(64)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(Tenants::DisplayName).string().not_null())
                    .col(ColumnDef::new(Tenants::ConnectionString).string().null())
                    .col(
                        ColumnDef::new(Tenants::IsActive)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(
                        ColumnDef::new(Tenants::RequireTwoFactor)
                            .boolean()
                            .not_null()
                            .default(false),
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
    RequireTwoFactor,
    AuditRetentionDays,
    DefaultCurrency,
    TaxPin,
    VatNumber,
    LogoPath,
    CreatedAt,
}

struct CreateUsers;

impl MigrationName for CreateUsers {
    fn name(&self) -> &str {
        "m20260709_000002_create_users"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateUsers {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Users::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(Users::Id).uuid().not_null().primary_key())
                    .col(ColumnDef::new(Users::TenantId).uuid().null())
                    .col(ColumnDef::new(Users::UserName).string_len(64).not_null())
                    .col(
                        ColumnDef::new(Users::NormalizedUserName)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(ColumnDef::new(Users::Email).string_len(255).not_null())
                    .col(
                        ColumnDef::new(Users::NormalizedEmail)
                            .string_len(255)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Users::EmailConfirmed)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Users::EmailConfirmationToken)
                            .string()
                            .null(),
                    )
                    .col(ColumnDef::new(Users::PasswordHash).text().not_null())
                    .col(
                        ColumnDef::new(Users::PasswordChangedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(ColumnDef::new(Users::PasswordResetToken).string().null())
                    .col(
                        ColumnDef::new(Users::PasswordResetExpiresAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(Users::SecurityStamp)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(ColumnDef::new(Users::FirstName).string_len(64).not_null())
                    .col(ColumnDef::new(Users::LastName).string_len(64).not_null())
                    .col(ColumnDef::new(Users::PhoneNumber).string_len(32).null())
                    .col(
                        ColumnDef::new(Users::PhoneNumberConfirmed)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Users::IsActive)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(
                        ColumnDef::new(Users::IsTenantAdmin)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Users::LockoutEnabled)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(
                        ColumnDef::new(Users::LockoutEndAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(Users::AccessFailedCount)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Users::TwoFactorEnabled)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(ColumnDef::new(Users::TotpSecret).string().null())
                    .col(
                        ColumnDef::new(Users::TotpConfirmedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(ColumnDef::new(Users::RecoveryCodes).text().null())
                    .col(
                        ColumnDef::new(Users::LastLoginAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(ColumnDef::new(Users::Language).string_len(16).null())
                    .col(ColumnDef::new(Users::TimeZone).string_len(64).null())
                    .col(
                        ColumnDef::new(Users::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(Users::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(Users::DeletedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("ux_users_tenant_user_name")
                    .if_not_exists()
                    .table(Users::Table)
                    .col(Users::TenantId)
                    .col(Users::NormalizedUserName)
                    .unique()
                    .nulls_not_distinct()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ux_users_tenant_email")
                    .if_not_exists()
                    .table(Users::Table)
                    .col(Users::TenantId)
                    .col(Users::NormalizedEmail)
                    .unique()
                    .nulls_not_distinct()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Users::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
    TenantId,
    UserName,
    NormalizedUserName,
    Email,
    NormalizedEmail,
    EmailConfirmed,
    EmailConfirmationToken,
    PasswordHash,
    PasswordChangedAt,
    PasswordResetToken,
    PasswordResetExpiresAt,
    SecurityStamp,
    FirstName,
    LastName,
    PhoneNumber,
    PhoneNumberConfirmed,
    IsActive,
    IsTenantAdmin,
    LockoutEnabled,
    LockoutEndAt,
    AccessFailedCount,
    TwoFactorEnabled,
    TotpSecret,
    TotpConfirmedAt,
    RecoveryCodes,
    LastLoginAt,
    Language,
    TimeZone,
    CreatedAt,
    UpdatedAt,
    DeletedAt,
}

struct CreateRefreshTokens;

impl MigrationName for CreateRefreshTokens {
    fn name(&self) -> &str {
        "m20260709_000003_create_refresh_tokens"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateRefreshTokens {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(RefreshTokens::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(RefreshTokens::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(RefreshTokens::UserId).uuid().not_null())
                    .col(
                        ColumnDef::new(RefreshTokens::TokenHash)
                            .string_len(64)
                            .not_null()
                            .unique_key(),
                    )
                    .col(
                        ColumnDef::new(RefreshTokens::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RefreshTokens::RevokedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(RefreshTokens::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("ix_refresh_tokens_user")
                    .if_not_exists()
                    .table(RefreshTokens::Table)
                    .col(RefreshTokens::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RefreshTokens::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum RefreshTokens {
    Table,
    Id,
    UserId,
    TokenHash,
    ExpiresAt,
    RevokedAt,
    CreatedAt,
}

struct CreateRolesAndPermissions;

impl MigrationName for CreateRolesAndPermissions {
    fn name(&self) -> &str {
        "m20260710_000004_create_roles_and_permissions"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateRolesAndPermissions {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Roles::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(Roles::Id).uuid().not_null().primary_key())
                    .col(ColumnDef::new(Roles::TenantId).uuid().null())
                    .col(ColumnDef::new(Roles::Name).string_len(64).not_null())
                    .col(ColumnDef::new(Roles::DisplayName).string().not_null())
                    .col(
                        ColumnDef::new(Roles::IsStatic)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Roles::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ux_roles_tenant_name")
                    .if_not_exists()
                    .table(Roles::Table)
                    .col(Roles::TenantId)
                    .col(Roles::Name)
                    .unique()
                    .nulls_not_distinct()
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(UserRoles::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(UserRoles::UserId).uuid().not_null())
                    .col(ColumnDef::new(UserRoles::RoleId).uuid().not_null())
                    .primary_key(
                        Index::create()
                            .col(UserRoles::UserId)
                            .col(UserRoles::RoleId),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(PermissionGrants::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PermissionGrants::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(PermissionGrants::Permission)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(ColumnDef::new(PermissionGrants::RoleId).uuid().null())
                    .col(ColumnDef::new(PermissionGrants::UserId).uuid().null())
                    .col(
                        ColumnDef::new(PermissionGrants::IsGranted)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_permission_grants_role")
                    .if_not_exists()
                    .table(PermissionGrants::Table)
                    .col(PermissionGrants::RoleId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_permission_grants_user")
                    .if_not_exists()
                    .table(PermissionGrants::Table)
                    .col(PermissionGrants::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PermissionGrants::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(UserRoles::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Roles::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Roles {
    Table,
    Id,
    TenantId,
    Name,
    DisplayName,
    IsStatic,
    CreatedAt,
}

#[derive(DeriveIden)]
enum UserRoles {
    Table,
    UserId,
    RoleId,
}

#[derive(DeriveIden)]
enum PermissionGrants {
    Table,
    Id,
    Permission,
    RoleId,
    UserId,
    IsGranted,
}

struct CreateAuditLogs;

impl MigrationName for CreateAuditLogs {
    fn name(&self) -> &str {
        "m20260710_000005_create_audit_logs"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateAuditLogs {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AuditLogs::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AuditLogs::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(AuditLogs::TenantId).uuid().null())
                    .col(ColumnDef::new(AuditLogs::UserId).uuid().null())
                    .col(ColumnDef::new(AuditLogs::RequestId).string_len(64).null())
                    .col(ColumnDef::new(AuditLogs::Method).string_len(16).not_null())
                    .col(ColumnDef::new(AuditLogs::Path).string().not_null())
                    .col(ColumnDef::new(AuditLogs::StatusCode).integer().null())
                    .col(ColumnDef::new(AuditLogs::IpAddress).string_len(64).null())
                    .col(ColumnDef::new(AuditLogs::UserAgent).string_len(512).null())
                    .col(ColumnDef::new(AuditLogs::DurationMs).big_integer().null())
                    .col(ColumnDef::new(AuditLogs::Action).string_len(16).not_null())
                    .col(ColumnDef::new(AuditLogs::EntityType).string_len(128).null())
                    .col(ColumnDef::new(AuditLogs::EntityId).string_len(64).null())
                    .col(ColumnDef::new(AuditLogs::OldValues).json_binary().null())
                    .col(ColumnDef::new(AuditLogs::NewValues).json_binary().null())
                    .col(
                        ColumnDef::new(AuditLogs::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_audit_logs_tenant_created")
                    .if_not_exists()
                    .table(AuditLogs::Table)
                    .col(AuditLogs::TenantId)
                    .col(AuditLogs::CreatedAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_audit_logs_entity")
                    .if_not_exists()
                    .table(AuditLogs::Table)
                    .col(AuditLogs::EntityType)
                    .col(AuditLogs::EntityId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_audit_logs_user")
                    .if_not_exists()
                    .table(AuditLogs::Table)
                    .col(AuditLogs::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(AuditLogs::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum AuditLogs {
    Table,
    Id,
    TenantId,
    UserId,
    RequestId,
    Method,
    Path,
    StatusCode,
    IpAddress,
    UserAgent,
    DurationMs,
    Action,
    EntityType,
    EntityId,
    Message,
    OldValues,
    NewValues,
    CreatedAt,
}

struct AddTenantAuditRetention;

impl MigrationName for AddTenantAuditRetention {
    fn name(&self) -> &str {
        "m20260710_000007_add_tenant_audit_retention"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddTenantAuditRetention {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::AuditRetentionDays).integer().null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .drop_column(Tenants::AuditRetentionDays)
                    .to_owned(),
            )
            .await
    }
}

/// The login directory: a main-database index of every tenant user's
/// normalized login identifiers, so sign-in can resolve which tenant a
/// set of credentials belongs to without a tenant header. Backfilled
/// from the users already present in this database.
struct CreateUserDirectory;

impl MigrationName for CreateUserDirectory {
    fn name(&self) -> &str {
        "m20260710_000008_create_user_directory"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateUserDirectory {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(UserDirectory::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(UserDirectory::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(UserDirectory::TenantId).uuid().not_null())
                    .col(ColumnDef::new(UserDirectory::UserId).uuid().not_null())
                    .col(
                        ColumnDef::new(UserDirectory::NormalizedUserName)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserDirectory::NormalizedEmail)
                            .string_len(255)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserDirectory::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ux_user_directory_tenant_user")
                    .if_not_exists()
                    .table(UserDirectory::Table)
                    .col(UserDirectory::TenantId)
                    .col(UserDirectory::UserId)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_user_directory_user_name")
                    .if_not_exists()
                    .table(UserDirectory::Table)
                    .col(UserDirectory::NormalizedUserName)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_user_directory_email")
                    .if_not_exists()
                    .table(UserDirectory::Table)
                    .col(UserDirectory::NormalizedEmail)
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "INSERT INTO user_directory \
                 (id, tenant_id, user_id, normalized_user_name, normalized_email) \
                 SELECT gen_random_uuid(), tenant_id, id, normalized_user_name, normalized_email \
                 FROM users WHERE tenant_id IS NOT NULL AND deleted_at IS NULL \
                 ON CONFLICT DO NOTHING",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(UserDirectory::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum UserDirectory {
    Table,
    Id,
    TenantId,
    UserId,
    NormalizedUserName,
    NormalizedEmail,
    CreatedAt,
}

/// The currency table, pre-populated with the world's currencies as
/// undeletable system rows. Deployments add their own units through the
/// currency module's endpoints; tenants pick a default from the list.
struct CreateCurrencies;

impl MigrationName for CreateCurrencies {
    fn name(&self) -> &str {
        "m20260710_000009_create_currencies"
    }
}

/// `(code, name, minor units)` — seeded as system currencies.
const SYSTEM_CURRENCIES: &[(&str, &str, i16)] = &[
    ("AED", "UAE Dirham", 2),
    ("AUD", "Australian Dollar", 2),
    ("BDT", "Bangladeshi Taka", 2),
    ("BHD", "Bahraini Dinar", 3),
    ("BIF", "Burundian Franc", 0),
    ("BRL", "Brazilian Real", 2),
    ("BWP", "Botswana Pula", 2),
    ("CAD", "Canadian Dollar", 2),
    ("CDF", "Congolese Franc", 2),
    ("CHF", "Swiss Franc", 2),
    ("CNY", "Chinese Yuan", 2),
    ("DJF", "Djiboutian Franc", 0),
    ("DKK", "Danish Krone", 2),
    ("EGP", "Egyptian Pound", 2),
    ("ERN", "Eritrean Nakfa", 2),
    ("ETB", "Ethiopian Birr", 2),
    ("EUR", "Euro", 2),
    ("GBP", "British Pound", 2),
    ("GHS", "Ghanaian Cedi", 2),
    ("HKD", "Hong Kong Dollar", 2),
    ("IDR", "Indonesian Rupiah", 2),
    ("ILS", "Israeli New Shekel", 2),
    ("INR", "Indian Rupee", 2),
    ("JPY", "Japanese Yen", 0),
    ("KES", "Kenyan Shilling", 2),
    ("KRW", "South Korean Won", 0),
    ("KWD", "Kuwaiti Dinar", 3),
    ("LKR", "Sri Lankan Rupee", 2),
    ("MAD", "Moroccan Dirham", 2),
    ("MUR", "Mauritian Rupee", 2),
    ("MWK", "Malawian Kwacha", 2),
    ("MXN", "Mexican Peso", 2),
    ("MYR", "Malaysian Ringgit", 2),
    ("MZN", "Mozambican Metical", 2),
    ("NGN", "Nigerian Naira", 2),
    ("NOK", "Norwegian Krone", 2),
    ("NZD", "New Zealand Dollar", 2),
    ("OMR", "Omani Rial", 3),
    ("PHP", "Philippine Peso", 2),
    ("PKR", "Pakistani Rupee", 2),
    ("PLN", "Polish Zloty", 2),
    ("QAR", "Qatari Riyal", 2),
    ("RUB", "Russian Ruble", 2),
    ("RWF", "Rwandan Franc", 0),
    ("SAR", "Saudi Riyal", 2),
    ("SEK", "Swedish Krona", 2),
    ("SGD", "Singapore Dollar", 2),
    ("SOS", "Somali Shilling", 2),
    ("SSP", "South Sudanese Pound", 2),
    ("THB", "Thai Baht", 2),
    ("TRY", "Turkish Lira", 2),
    ("TZS", "Tanzanian Shilling", 2),
    ("UGX", "Ugandan Shilling", 0),
    ("USD", "US Dollar", 2),
    ("VND", "Vietnamese Dong", 0),
    ("XAF", "Central African CFA Franc", 0),
    ("XOF", "West African CFA Franc", 0),
    ("ZAR", "South African Rand", 2),
    ("ZMW", "Zambian Kwacha", 2),
];

#[async_trait::async_trait]
impl MigrationTrait for CreateCurrencies {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Currencies::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Currencies::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Currencies::Code)
                            .string_len(8)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(Currencies::Name).string_len(64).not_null())
                    .col(
                        ColumnDef::new(Currencies::MinorUnits)
                            .small_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Currencies::IsSystem)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Currencies::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        let values = SYSTEM_CURRENCIES
            .iter()
            .map(|(code, name, minor)| {
                format!("(gen_random_uuid(), '{code}', '{name}', {minor}, TRUE)")
            })
            .collect::<Vec<_>>()
            .join(", ");
        manager
            .get_connection()
            .execute_unprepared(&format!(
                "INSERT INTO currencies (id, code, name, minor_units, is_system) \
                 VALUES {values} ON CONFLICT (code) DO NOTHING"
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Currencies::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Currencies {
    Table,
    Id,
    Code,
    Name,
    MinorUnits,
    IsSystem,
    CreatedAt,
}

/// Company profile fields on the tenant: default currency, tax
/// registration identifiers and the uploaded logo's storage path.
struct AddTenantCompanyProfile;

impl MigrationName for AddTenantCompanyProfile {
    fn name(&self) -> &str {
        "m20260710_000010_add_tenant_company_profile"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddTenantCompanyProfile {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::DefaultCurrency)
                            .string_len(8)
                            .null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::TaxPin).string_len(64).null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::VatNumber).string_len(64).null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::LogoPath).string_len(255).null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .drop_column(Tenants::DefaultCurrency)
                    .drop_column(Tenants::TaxPin)
                    .drop_column(Tenants::VatNumber)
                    .drop_column(Tenants::LogoPath)
                    .to_owned(),
            )
            .await
    }
}

struct AddAuditLogMessage;

impl MigrationName for AddAuditLogMessage {
    fn name(&self) -> &str {
        "m20260710_000006_add_audit_log_message"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddAuditLogMessage {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(AuditLogs::Table)
                    .add_column_if_not_exists(ColumnDef::new(AuditLogs::Message).string().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(AuditLogs::Table)
                    .drop_column(AuditLogs::Message)
                    .to_owned(),
            )
            .await
    }
}
