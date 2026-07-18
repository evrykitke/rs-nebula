//! User lifecycle management over one database (the main database in
//! host/single-tenant context, or a tenant's own database).
//!
//! Security posture:
//! - failed logins are counted and trip a temporary lockout (423);
//! - wrong login and wrong password are indistinguishable (401) to
//!   prevent user enumeration;
//! - the security stamp rotates on any credential change, invalidating
//!   outstanding tokens;
//! - soft-deleted users are invisible to every lookup.

use super::policy::PasswordPolicy;
use super::{password, password_history, refresh_token, totp, user};
use crate::config::AuthConfig;
use crate::error::{Error, Result};
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub struct NewUser {
    pub tenant_id: Option<Uuid>,
    pub user_name: String,
    pub email: String,
    pub password: String,
    pub first_name: String,
    pub last_name: String,
    pub is_tenant_admin: bool,
    pub language: Option<String>,
    pub time_zone: Option<String>,
    pub phone_number: Option<String>,
}

/// Returned once from two-factor setup; the secret and URL feed the
/// authenticator app, the recovery codes are shown a single time.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TwoFactorSetup {
    pub secret: String,
    pub otpauth_url: String,
}

pub struct UserManager {
    db: DatabaseConnection,
    config: AuthConfig,
    directory: Option<super::directory::Directory>,
    policy: PasswordPolicy,
}

impl UserManager {
    /// Without a tenant policy attached, the deployment's own `auth.*`
    /// settings are the policy — which is exactly right for host users
    /// and single-tenant mode, where there is no company to tighten them.
    pub fn new(db: DatabaseConnection, config: AuthConfig) -> Self {
        let policy = PasswordPolicy::from_config(&config);
        Self {
            db,
            config,
            directory: None,
            policy,
        }
    }

    /// Keep the main-database login directory in sync with this user
    /// store — required in multitenant mode so credential-based sign-in
    /// can resolve which tenant a login belongs to.
    pub fn with_directory(mut self, main: DatabaseConnection) -> Self {
        self.directory = Some(super::directory::Directory::new(main));
        self
    }

    /// Hold this store to a company's password policy instead of the
    /// deployment default.
    pub fn with_policy(mut self, policy: PasswordPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn policy(&self) -> &PasswordPolicy {
        &self.policy
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub async fn create(&self, new: NewUser) -> Result<user::Model> {
        validate_new_user(&new, &self.policy)?;

        let normalized_user_name = normalize(&new.user_name);
        let normalized_email = normalize(&new.email);
        let taken = user::Entity::find()
            .filter(tenant_filter(new.tenant_id))
            .filter(
                user::Column::NormalizedUserName
                    .eq(normalized_user_name.clone())
                    .or(user::Column::NormalizedEmail.eq(normalized_email.clone())),
            )
            .one(&self.db)
            .await?;
        if taken.is_some() {
            return Err(Error::Conflict(
                "a user with that username or email already exists".into(),
            ));
        }

        let now = Utc::now();
        let user = user::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(new.tenant_id),
            user_name: Set(new.user_name),
            normalized_user_name: Set(normalized_user_name),
            email: Set(new.email),
            normalized_email: Set(normalized_email),
            email_confirmed: Set(false),
            email_confirmation_token: Set(Some(random_token())),
            password_hash: Set(password::hash(&new.password)?),
            password_changed_at: Set(Some(now)),
            security_stamp: Set(random_token()),
            first_name: Set(new.first_name),
            last_name: Set(new.last_name),
            phone_number: Set(new.phone_number),
            phone_number_confirmed: Set(false),
            is_active: Set(true),
            is_tenant_admin: Set(new.is_tenant_admin),
            lockout_enabled: Set(true),
            access_failed_count: Set(0),
            two_factor_enabled: Set(false),
            language: Set(new.language),
            time_zone: Set(new.time_zone),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)?;

        if let (Some(directory), Some(tenant_id)) = (&self.directory, user.tenant_id) {
            directory.add(tenant_id, &user).await?;
        }
        Ok(user)
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<user::Model>> {
        Ok(user::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .filter(|u| u.deleted_at.is_none()))
    }

    /// Look up by username or email, scoped to the tenant.
    pub async fn find_by_login(
        &self,
        tenant_id: Option<Uuid>,
        login: &str,
    ) -> Result<Option<user::Model>> {
        let needle = normalize(login);
        Ok(user::Entity::find()
            .filter(tenant_filter(tenant_id))
            .filter(
                user::Column::NormalizedUserName
                    .eq(needle.clone())
                    .or(user::Column::NormalizedEmail.eq(needle)),
            )
            .one(&self.db)
            .await?
            .filter(|u| u.deleted_at.is_none()))
    }

    /// Password check with lockout accounting. Success resets the failure
    /// counter and records the login.
    pub async fn authenticate(
        &self,
        tenant_id: Option<Uuid>,
        login: &str,
        pass: &str,
    ) -> Result<user::Model> {
        let Some(user) = self.find_by_login(tenant_id, login).await? else {
            // Hash anyway so the timing matches the found-user path.
            let _ = password::hash(pass);
            return Err(Error::Unauthorized);
        };

        if let Some(until) = user.lockout_end_at {
            if until > Utc::now() {
                return Err(Error::Locked(format!(
                    "account is locked until {}",
                    until.to_rfc3339()
                )));
            }
        }
        if !user.is_active {
            return Err(Error::Unauthorized);
        }

        if !password::verify(pass, &user.password_hash) {
            let failed = user.access_failed_count + 1;
            let mut active: user::ActiveModel = user.clone().into();
            active.access_failed_count = Set(failed);
            if user.lockout_enabled && failed >= self.policy.lockout_max_failed {
                active.lockout_end_at = Set(Some(
                    Utc::now() + Duration::seconds(self.policy.lockout_secs as i64),
                ));
                active.access_failed_count = Set(0);
            }
            active.updated_at = Set(Utc::now());
            active.update(&self.db).await?;
            return Err(Error::Unauthorized);
        }

        let mut active: user::ActiveModel = user.into();
        active.access_failed_count = Set(0);
        active.lockout_end_at = Set(None);
        active.last_login_at = Set(Some(Utc::now()));
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    pub async fn change_password(
        &self,
        user: user::Model,
        current: &str,
        new_password: &str,
    ) -> Result<user::Model> {
        if !password::verify(current, &user.password_hash) {
            return Err(Error::Unauthorized);
        }
        self.replace_password(user, new_password).await
    }

    /// Set a password without re-checking the current one, for flows that
    /// have already proved the caller holds it — the forced change after
    /// an expiry, where the password being replaced is the one just used
    /// to sign in.
    pub async fn replace_password(
        &self,
        user: user::Model,
        new_password: &str,
    ) -> Result<user::Model> {
        self.policy.check(new_password)?;
        self.refuse_reused_password(&user, new_password).await?;

        let user_id = user.id;
        let retired = user.password_hash.clone();
        let mut active: user::ActiveModel = user.into();
        active.password_hash = Set(password::hash(new_password)?);
        active.password_changed_at = Set(Some(Utc::now()));
        active.security_stamp = Set(random_token());
        active.updated_at = Set(Utc::now());
        let user = active.update(&self.db).await?;

        self.retire_password(user_id, retired).await?;
        self.revoke_all_refresh_tokens(user_id).await?;
        Ok(user)
    }

    /// Refuse a password the user is still supposed to be moving away
    /// from. The current password counts as the most recent of "your last
    /// N" — a policy that let you re-set the password you already have
    /// would defeat the expiry it exists to serve.
    ///
    /// Each candidate costs a full Argon2 verification, which is
    /// deliberately slow. That is affordable here only because N is small
    /// and password changes are rare; this is not a loop to widen.
    async fn refuse_reused_password(&self, user: &user::Model, candidate: &str) -> Result<()> {
        let keep = self.policy.history_count;
        if keep == 0 {
            return Ok(());
        }
        if password::verify(candidate, &user.password_hash) {
            return Err(reuse_error(keep));
        }
        let previous = password_history::Entity::find()
            .filter(password_history::Column::UserId.eq(user.id))
            .order_by_desc(password_history::Column::CreatedAt)
            .limit((keep - 1) as u64)
            .all(&self.db)
            .await?;
        if previous
            .iter()
            .any(|h| password::verify(candidate, &h.password_hash))
        {
            return Err(reuse_error(keep));
        }
        Ok(())
    }

    /// File the hash of a password the user has just left behind, and drop
    /// whatever has aged out of the policy's window so the table stays
    /// bounded. A tenant that does not check history keeps no history.
    async fn retire_password(&self, user_id: Uuid, hash: String) -> Result<()> {
        if self.policy.history_count == 0 {
            return Ok(());
        }
        password_history::ActiveModel {
            id: Set(Uuid::new_v4()),
            user_id: Set(user_id),
            password_hash: Set(hash),
            created_at: Set(Utc::now()),
        }
        .insert(&self.db)
        .await?;

        let keep = password_history::Entity::find()
            .filter(password_history::Column::UserId.eq(user_id))
            .order_by_desc(password_history::Column::CreatedAt)
            .limit(self.policy.history_count as u64)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|h| h.id)
            .collect::<Vec<_>>();
        password_history::Entity::delete_many()
            .filter(password_history::Column::UserId.eq(user_id))
            .filter(password_history::Column::Id.is_not_in(keep))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Whether this user's password has aged past the company's policy and
    /// must be changed before a session is issued.
    pub fn password_expired(&self, user: &user::Model) -> bool {
        self.policy.expired(user.password_changed_at)
    }

    pub async fn confirm_email(&self, user: user::Model, token: &str) -> Result<user::Model> {
        if user.email_confirmation_token.as_deref() != Some(token) {
            return Err(Error::Validation("invalid confirmation token".into()));
        }
        let mut active: user::ActiveModel = user.into();
        active.email_confirmed = Set(true);
        active.email_confirmation_token = Set(None);
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    pub async fn set_active(&self, user: user::Model, is_active: bool) -> Result<user::Model> {
        let user_id = user.id;
        let mut active: user::ActiveModel = user.into();
        active.is_active = Set(is_active);
        active.security_stamp = Set(random_token());
        active.updated_at = Set(Utc::now());
        let user = active.update(&self.db).await?;
        self.revoke_all_refresh_tokens(user_id).await?;
        Ok(user)
    }

    pub async fn soft_delete(&self, user: user::Model) -> Result<()> {
        let user_id = user.id;
        let tenant_id = user.tenant_id;
        let mut active: user::ActiveModel = user.into();
        active.deleted_at = Set(Some(Utc::now()));
        active.security_stamp = Set(random_token());
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;
        self.revoke_all_refresh_tokens(user_id).await?;
        if let (Some(directory), Some(tenant_id)) = (&self.directory, tenant_id) {
            directory.remove(tenant_id, user_id).await?;
        }
        Ok(())
    }

    /// Step 1 of enabling an authenticator app: store an unconfirmed
    /// secret and hand back the provisioning URL. Idempotent until
    /// confirmed — calling again issues a fresh secret.
    pub async fn begin_two_factor_setup(
        &self,
        user: user::Model,
    ) -> Result<(user::Model, TwoFactorSetup)> {
        let secret = totp::generate_secret();
        let url = totp::provisioning_url(&secret, &self.config.totp_issuer, &user.email)?;
        let mut active: user::ActiveModel = user.into();
        active.totp_secret = Set(Some(secret.clone()));
        active.totp_confirmed_at = Set(None);
        active.updated_at = Set(Utc::now());
        let user = active.update(&self.db).await?;
        Ok((
            user,
            TwoFactorSetup {
                secret,
                otpauth_url: url,
            },
        ))
    }

    /// Step 2: the user proves the authenticator works. Enables 2FA and
    /// returns the recovery codes — the only time they are visible.
    pub async fn confirm_two_factor(
        &self,
        user: user::Model,
        code: &str,
    ) -> Result<(user::Model, Vec<String>)> {
        let Some(secret) = user.totp_secret.clone() else {
            return Err(Error::Validation(
                "two-factor setup has not been started".into(),
            ));
        };
        if !totp::verify_code(&secret, code)? {
            return Err(Error::Validation("invalid authenticator code".into()));
        }
        let codes = totp::generate_recovery_codes();
        let user_id = user.id;
        let mut active: user::ActiveModel = user.into();
        active.two_factor_enabled = Set(true);
        active.totp_confirmed_at = Set(Some(Utc::now()));
        active.recovery_codes = Set(Some(totp::hash_recovery_codes(&codes)));
        active.security_stamp = Set(random_token());
        active.updated_at = Set(Utc::now());
        let user = active.update(&self.db).await?;
        self.revoke_all_refresh_tokens(user_id).await?;
        Ok((user, codes))
    }

    /// Accepts an authenticator code, or consumes a recovery code.
    pub async fn verify_two_factor(&self, user: user::Model, code: &str) -> Result<user::Model> {
        let confirmed = user.totp_confirmed_at.is_some();
        if let (true, Some(secret)) = (confirmed, user.totp_secret.as_deref()) {
            if totp::verify_code(secret, code)? {
                return Ok(user);
            }
        }
        if let Some(stored) = user.recovery_codes.as_deref() {
            if let Some(remaining) = totp::consume_recovery_code(stored, code) {
                let mut active: user::ActiveModel = user.into();
                active.recovery_codes = Set(Some(remaining));
                active.updated_at = Set(Utc::now());
                return active.update(&self.db).await.map_err(Error::from);
            }
        }
        Err(Error::Unauthorized)
    }

    pub async fn disable_two_factor(&self, user: user::Model) -> Result<user::Model> {
        let user_id = user.id;
        let mut active: user::ActiveModel = user.into();
        active.two_factor_enabled = Set(false);
        active.totp_secret = Set(None);
        active.totp_confirmed_at = Set(None);
        active.recovery_codes = Set(None);
        active.security_stamp = Set(random_token());
        active.updated_at = Set(Utc::now());
        let user = active.update(&self.db).await?;
        self.revoke_all_refresh_tokens(user_id).await?;
        Ok(user)
    }

    /// Everyone in a tenant, for the admin user list (soft-deleted
    /// excluded).
    pub async fn find_all(&self, tenant_id: Option<Uuid>) -> Result<Vec<user::Model>> {
        user::Entity::find()
            .filter(tenant_filter(tenant_id))
            .filter(user::Column::DeletedAt.is_null())
            .order_by_asc(user::Column::Id)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    /// Grant or revoke tenant-admin. The registering user starts as the
    /// admin, but this is how it changes hands later.
    pub async fn set_tenant_admin(&self, user: user::Model, is_admin: bool) -> Result<user::Model> {
        let mut active: user::ActiveModel = user.into();
        active.is_tenant_admin = Set(is_admin);
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Set (or clear, with `None`) the user's override PIN — the short
    /// numeric credential supervised acts are approved with. Only the
    /// Argon2 hash is stored.
    pub async fn set_override_pin(
        &self,
        user: user::Model,
        pin: Option<&str>,
    ) -> Result<user::Model> {
        let hash = match pin {
            Some(p) => Some(password::hash(p)?),
            None => None,
        };
        let mut active: user::ActiveModel = user.into();
        active.override_pin_hash = Set(hash);
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Issue a refresh token: 48 random bytes, returned raw exactly once,
    /// stored only as a SHA-256 hash.
    pub async fn issue_refresh_token(&self, user_id: Uuid) -> Result<String> {
        let mut bytes = [0u8; 48];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        let raw = hex::encode(bytes);
        refresh_token::ActiveModel {
            id: Set(Uuid::new_v4()),
            user_id: Set(user_id),
            token_hash: Set(hash_token(&raw)),
            expires_at: Set(
                Utc::now() + Duration::seconds(self.config.refresh_token_ttl_secs as i64)
            ),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(raw)
    }

    /// Rotate a refresh token: the presented token is revoked and a new
    /// one issued. Presenting an already-revoked token is treated as
    /// theft — every session of that user is revoked.
    pub async fn rotate_refresh_token(&self, raw: &str) -> Result<(user::Model, String)> {
        let found = refresh_token::Entity::find()
            .filter(refresh_token::Column::TokenHash.eq(hash_token(raw)))
            .one(&self.db)
            .await?;
        let Some(token) = found else {
            return Err(Error::Unauthorized);
        };

        if token.revoked_at.is_some() {
            tracing::warn!(
                user_id = %token.user_id,
                "revoked refresh token reused; revoking all sessions"
            );
            self.revoke_all_refresh_tokens(token.user_id).await?;
            return Err(Error::Unauthorized);
        }
        if token.expires_at < Utc::now() {
            return Err(Error::Unauthorized);
        }

        let user = self
            .find_by_id(token.user_id)
            .await?
            .filter(|u| u.is_active)
            .ok_or(Error::Unauthorized)?;

        let user_id = token.user_id;
        let mut active: refresh_token::ActiveModel = token.into();
        active.revoked_at = Set(Some(Utc::now()));
        active.update(&self.db).await?;

        let new_raw = self.issue_refresh_token(user_id).await?;
        Ok((user, new_raw))
    }

    /// Logout: revoke one refresh token. Unknown tokens are ignored so
    /// logout is idempotent.
    pub async fn revoke_refresh_token(&self, raw: &str) -> Result<()> {
        let found = refresh_token::Entity::find()
            .filter(refresh_token::Column::TokenHash.eq(hash_token(raw)))
            .one(&self.db)
            .await?;
        if let Some(token) = found {
            if token.revoked_at.is_none() {
                let mut active: refresh_token::ActiveModel = token.into();
                active.revoked_at = Set(Some(Utc::now()));
                active.update(&self.db).await?;
            }
        }
        Ok(())
    }

    pub async fn revoke_all_refresh_tokens(&self, user_id: Uuid) -> Result<()> {
        refresh_token::Entity::update_many()
            .col_expr(
                refresh_token::Column::RevokedAt,
                sea_orm::sea_query::Expr::value(Utc::now()),
            )
            .filter(refresh_token::Column::UserId.eq(user_id))
            .filter(refresh_token::Column::RevokedAt.is_null())
            .exec(&self.db)
            .await
            .map(|_| ())
            .map_err(Error::from)
    }
}

fn reuse_error(keep: u32) -> Error {
    Error::Validation(match keep {
        1 => "the new password must differ from the current one".into(),
        n => format!("the new password must differ from your last {n} passwords"),
    })
}

/// The pure field checks of [`UserManager::create`], usable before any
/// row exists. Registration runs this ahead of provisioning a tenant, so
/// a bad email or weak password fails before a database is cut for it.
pub(crate) fn validate_new_user(new: &NewUser, policy: &PasswordPolicy) -> Result<()> {
    validate_user_name(&new.user_name)?;
    validate_email(&new.email)?;
    policy.check(&new.password)?;
    if !new.first_name.trim().is_empty() && new.first_name.len() > 64
        || !new.last_name.trim().is_empty() && new.last_name.len() > 64
    {
        return Err(Error::Validation(
            "names are limited to 64 characters".into(),
        ));
    }
    Ok(())
}

fn hash_token(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

fn tenant_filter(tenant_id: Option<Uuid>) -> sea_orm::Condition {
    match tenant_id {
        Some(id) => sea_orm::Condition::all().add(user::Column::TenantId.eq(id)),
        None => sea_orm::Condition::all().add(user::Column::TenantId.is_null()),
    }
}

pub(crate) fn normalize(value: &str) -> String {
    value.trim().to_uppercase()
}

fn random_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn validate_user_name(name: &str) -> Result<()> {
    let ok = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+'));
    if !ok {
        return Err(Error::Validation(
            "username must be 1-64 letters, digits or . - _ @ +".into(),
        ));
    }
    Ok(())
}

fn validate_email(email: &str) -> Result<()> {
    let ok = email.len() <= 255
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        });
    if !ok {
        return Err(Error::Validation(format!(
            "{email:?} is not a valid email address"
        )));
    }
    Ok(())
}
