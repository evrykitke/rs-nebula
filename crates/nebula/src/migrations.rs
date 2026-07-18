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
            Box::new(CreateDocumentNumberCounters),
            Box::new(CreateDocumentSeries),
            Box::new(CreateReportSettings),
            Box::new(CreateReportJobs),
            Box::new(AddTenantCompanyContact),
            Box::new(AddReportJobParams),
            Box::new(AddTenantPasswordPolicy),
            Box::new(CreatePasswordHistory),
            Box::new(CreateTenantMailSettings),
            Box::new(AddUserOverridePin),
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
    Address,
    Email,
    Website,
    Phone,
    PasswordMinLength,
    PasswordRequireUppercase,
    PasswordRequireLowercase,
    PasswordRequireDigit,
    PasswordRequireSymbol,
    PasswordExpiryDays,
    PasswordHistoryCount,
    LockoutMaxFailed,
    LockoutSecs,
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
    OverridePinHash,
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

/// Per-database counters backing [`crate::numbering`]. One row per
/// `(series, period)`; the numbering primitive increments it inside the
/// caller's transaction so document sequences stay gap-free. Runs on the
/// main database and every tenant database, like the rest of the schema.
struct CreateDocumentNumberCounters;

impl MigrationName for CreateDocumentNumberCounters {
    fn name(&self) -> &str {
        "m20260711_000011_create_document_number_counters"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateDocumentNumberCounters {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DocumentNumberCounters::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DocumentNumberCounters::SeriesKey)
                            .string_len(64)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DocumentNumberCounters::Period)
                            .string_len(16)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DocumentNumberCounters::CurrentValue)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(DocumentNumberCounters::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .primary_key(
                        Index::create()
                            .col(DocumentNumberCounters::SeriesKey)
                            .col(DocumentNumberCounters::Period),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(DocumentNumberCounters::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum DocumentNumberCounters {
    Table,
    SeriesKey,
    Period,
    CurrentValue,
    UpdatedAt,
}

/// Per-database overrides for [`crate::numbering`] series. Empty by
/// default: a series falls back to the format its module declared in
/// code, so a tenant that never touches configuration still gets working
/// numbers. A row here lets a tenant override the template and reset
/// policy of one series. Runs on the main and every tenant database.
struct CreateDocumentSeries;

impl MigrationName for CreateDocumentSeries {
    fn name(&self) -> &str {
        "m20260711_000012_create_document_series"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateDocumentSeries {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DocumentSeries::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DocumentSeries::SeriesKey)
                            .string_len(64)
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DocumentSeries::Template)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DocumentSeries::Reset)
                            .string_len(16)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DocumentSeries::UpdatedAt)
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
            .drop_table(Table::drop().table(DocumentSeries::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum DocumentSeries {
    Table,
    SeriesKey,
    Template,
    Reset,
    UpdatedAt,
}

struct CreateReportSettings;

impl MigrationName for CreateReportSettings {
    fn name(&self) -> &str {
        "m20260712_000013_create_report_settings"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateReportSettings {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // A single-row table (id is always 1) holding the tenant's report
        // preferences — house format and watermark. Per database, so each
        // provisioned tenant keeps its own.
        manager
            .create_table(
                Table::create()
                    .table(ReportSettings::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ReportSettings::Id)
                            .small_integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ReportSettings::DefaultFormat).string_len(16).null())
                    .col(ColumnDef::new(ReportSettings::Watermark).string_len(255).null())
                    .col(
                        ColumnDef::new(ReportSettings::UpdatedAt)
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
            .drop_table(Table::drop().table(ReportSettings::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ReportSettings {
    Table,
    Id,
    DefaultFormat,
    Watermark,
    UpdatedAt,
}

/// Async report generation: one row per queued render. Data-heavy reports
/// are enqueued instead of rendered on the request thread; a worker builds
/// the document, stores the artifact, and updates this row. Per database,
/// like the rest of the reporting schema, so each tenant keeps its own job
/// history and artifacts.
struct CreateReportJobs;

impl MigrationName for CreateReportJobs {
    fn name(&self) -> &str {
        "m20260712_000014_create_report_jobs"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateReportJobs {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ReportJobs::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(ReportJobs::Id).uuid().not_null().primary_key())
                    .col(ColumnDef::new(ReportJobs::Report).string_len(64).not_null())
                    .col(ColumnDef::new(ReportJobs::Format).string_len(16).null())
                    .col(ColumnDef::new(ReportJobs::Output).string_len(16).not_null())
                    .col(ColumnDef::new(ReportJobs::Status).string_len(16).not_null())
                    .col(ColumnDef::new(ReportJobs::FilePath).string_len(512).null())
                    .col(ColumnDef::new(ReportJobs::ContentType).string_len(128).null())
                    .col(ColumnDef::new(ReportJobs::Extension).string_len(16).null())
                    .col(ColumnDef::new(ReportJobs::FileName).string_len(255).null())
                    .col(ColumnDef::new(ReportJobs::ByteSize).big_integer().null())
                    .col(ColumnDef::new(ReportJobs::Error).text().null())
                    .col(ColumnDef::new(ReportJobs::RequestedBy).string_len(255).null())
                    .col(ColumnDef::new(ReportJobs::RequestedById).uuid().null())
                    .col(
                        ColumnDef::new(ReportJobs::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(ReportJobs::StartedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(ReportJobs::CompletedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_report_jobs_created")
                    .if_not_exists()
                    .table(ReportJobs::Table)
                    .col(ReportJobs::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ReportJobs::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ReportJobs {
    Table,
    Id,
    Report,
    Format,
    Output,
    Status,
    Params,
    FilePath,
    ContentType,
    Extension,
    FileName,
    ByteSize,
    Error,
    RequestedBy,
    RequestedById,
    CreatedAt,
    StartedAt,
    CompletedAt,
}

/// The arguments a queued report was asked for, as JSON. A parameterized
/// report renders a different document per argument set, so a job that did
/// not carry them would quietly render the wrong one when the worker picked
/// it up. Null for the reports that take none.
struct AddReportJobParams;

impl MigrationName for AddReportJobParams {
    fn name(&self) -> &str {
        "m20260716_000016_add_report_job_params"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddReportJobParams {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ReportJobs::Table)
                    .add_column_if_not_exists(ColumnDef::new(ReportJobs::Params).text().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ReportJobs::Table)
                    .drop_column(ReportJobs::Params)
                    .to_owned(),
            )
            .await
    }
}

/// Company contact details shown on report chrome (running header/footer)
/// and the company profile: postal address, email, website and phone.
struct AddTenantCompanyContact;

impl MigrationName for AddTenantCompanyContact {
    fn name(&self) -> &str {
        "m20260712_000015_add_tenant_company_contact"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddTenantCompanyContact {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .add_column_if_not_exists(ColumnDef::new(Tenants::Address).string_len(512).null())
                    .add_column_if_not_exists(ColumnDef::new(Tenants::Email).string_len(255).null())
                    .add_column_if_not_exists(ColumnDef::new(Tenants::Website).string_len(255).null())
                    .add_column_if_not_exists(ColumnDef::new(Tenants::Phone).string_len(64).null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .drop_column(Tenants::Address)
                    .drop_column(Tenants::Email)
                    .drop_column(Tenants::Website)
                    .drop_column(Tenants::Phone)
                    .to_owned(),
            )
            .await
    }
}

/// The company's password policy. Every column is nullable and null means
/// "use the deployment default from `auth.*`" — the same override shape as
/// `audit_retention_days`, so a tenant that never opens the settings page
/// keeps whatever the deployment chose, and a policy tightened in config
/// reaches those tenants without a data migration.
///
/// These live on the tenant row because the login path already loads it;
/// a policy in the tenant's own database would be unreadable at exactly
/// the moment it is needed, before a session exists.
struct AddTenantPasswordPolicy;

impl MigrationName for AddTenantPasswordPolicy {
    fn name(&self) -> &str {
        "m20260717_000017_add_tenant_password_policy"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddTenantPasswordPolicy {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordMinLength).integer().null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordRequireUppercase)
                            .boolean()
                            .null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordRequireLowercase)
                            .boolean()
                            .null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordRequireDigit).boolean().null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordRequireSymbol)
                            .boolean()
                            .null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordExpiryDays).integer().null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::PasswordHistoryCount).integer().null(),
                    )
                    .add_column_if_not_exists(
                        ColumnDef::new(Tenants::LockoutMaxFailed).integer().null(),
                    )
                    .add_column_if_not_exists(ColumnDef::new(Tenants::LockoutSecs).integer().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Tenants::Table)
                    .drop_column(Tenants::PasswordMinLength)
                    .drop_column(Tenants::PasswordRequireUppercase)
                    .drop_column(Tenants::PasswordRequireLowercase)
                    .drop_column(Tenants::PasswordRequireDigit)
                    .drop_column(Tenants::PasswordRequireSymbol)
                    .drop_column(Tenants::PasswordExpiryDays)
                    .drop_column(Tenants::PasswordHistoryCount)
                    .drop_column(Tenants::LockoutMaxFailed)
                    .drop_column(Tenants::LockoutSecs)
                    .to_owned(),
            )
            .await
    }
}

/// Hashes of passwords a user has retired, so a reuse policy can refuse
/// them. Only hashes are kept — this table can no more reveal an old
/// password than the user table can reveal the current one. Rows past the
/// policy's window are deleted on each change, so the table stays bounded.
struct CreatePasswordHistory;

impl MigrationName for CreatePasswordHistory {
    fn name(&self) -> &str {
        "m20260717_000018_create_password_history"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreatePasswordHistory {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(PasswordHistory::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PasswordHistory::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(PasswordHistory::UserId).uuid().not_null())
                    .col(
                        ColumnDef::new(PasswordHistory::PasswordHash)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PasswordHistory::CreatedAt)
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
                    .name("ix_password_history_user")
                    .if_not_exists()
                    .table(PasswordHistory::Table)
                    .col(PasswordHistory::UserId)
                    .col(PasswordHistory::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PasswordHistory::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum PasswordHistory {
    Table,
    Id,
    UserId,
    PasswordHash,
    CreatedAt,
}

/// A tenant's own SMTP server, so one deployment can send as many
/// different companies. Its own table rather than more columns on
/// `tenants`: mail is read only when something is sent, whereas the tenant
/// row is read on every login, and a live credential should not ride along
/// with it.
///
/// `password_encrypted` holds AES-GCM ciphertext (see [`crate::crypto`]),
/// never the password. It is write-only across the API — the settings
/// endpoint reports whether one is set, never what it is.
struct CreateTenantMailSettings;

impl MigrationName for CreateTenantMailSettings {
    fn name(&self) -> &str {
        "m20260717_000019_create_tenant_mail_settings"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateTenantMailSettings {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(TenantMailSettings::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TenantMailSettings::TenantId)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::Host)
                            .string_len(255)
                            .not_null(),
                    )
                    .col(ColumnDef::new(TenantMailSettings::Port).integer().not_null())
                    .col(
                        ColumnDef::new(TenantMailSettings::Username)
                            .string_len(255)
                            .null(),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::PasswordEncrypted)
                            .text()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::Encryption)
                            .string_len(16)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::FromAddress)
                            .string_len(255)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::FromName)
                            .string_len(255)
                            .null(),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::Enabled)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(
                        ColumnDef::new(TenantMailSettings::UpdatedAt)
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
            .drop_table(Table::drop().table(TenantMailSettings::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum TenantMailSettings {
    Table,
    TenantId,
    Host,
    Port,
    Username,
    PasswordEncrypted,
    Encryption,
    FromAddress,
    FromName,
    Enabled,
    UpdatedAt,
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

/// The short numeric override PIN a supervisor keys in to approve gated
/// acts (POS voids, discounts, price overrides). Stored as an Argon2
/// hash, exactly like the password; a user without one simply cannot
/// approve by PIN.
struct AddUserOverridePin;

impl MigrationName for AddUserOverridePin {
    fn name(&self) -> &str {
        "m20260718_000020_add_user_override_pin"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for AddUserOverridePin {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Users::Table)
                    .add_column_if_not_exists(
                        ColumnDef::new(Users::OverridePinHash).string().null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Users::Table)
                    .drop_column(Users::OverridePinHash)
                    .to_owned(),
            )
            .await
    }
}
