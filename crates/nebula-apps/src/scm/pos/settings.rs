//! Tenant-wide POS behaviour: one row per tenant database.
//!
//! - **blind_count** — hide the expected cash from the cashier until they
//!   have counted (the classic honesty device). It binds the cashier only:
//!   a caller who can read POS reports sees the expectation regardless,
//!   and the server always validates counted-vs-expected itself.
//! - **denominations** — the note/coin set the till's count sheet offers,
//!   defaulting to the KES set. Purely a helper vocabulary: the server
//!   never assumes a drawer holds only these.
//! - **require_mpesa_reference** — whether capture refuses an M-Pesa tender
//!   without its confirmation code. On by default; a tenant that values
//!   queue speed over reconciliation rigour turns it off.
//! - **receipt_*** — the paper the tills print receipts to (width, margin,
//!   type size). The client renders to these; the server just keeps them.

use crate::scm::pos::permissions::names;
use axum::routing::get;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// The KES note/coin set, largest first — what a tenant gets until it says
/// otherwise.
const DEFAULT_DENOMINATIONS: &[i64] = &[1000, 500, 200, 100, 50, 20, 10, 5, 1];

/// The singleton row; the CHECK-ed boolean key holds it to one.
pub mod row {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "pos_settings")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: bool,
        pub blind_count: bool,
        #[sea_orm(column_type = "JsonBinary")]
        pub denominations: serde_json::Value,
        pub require_mpesa_reference: bool,
        pub receipt_paper_width_mm: i32,
        pub receipt_margin_mm: i32,
        pub receipt_font_size_px: i32,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// The settings as the API speaks them.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Settings {
    pub blind_count: bool,
    /// Largest first.
    #[schema(value_type = Vec<String>)]
    pub denominations: Vec<Decimal>,
    /// An M-Pesa tender must carry its confirmation code.
    pub require_mpesa_reference: bool,
    /// The roll the tills print to — 58 and 80 are the common widths.
    pub receipt_paper_width_mm: i32,
    pub receipt_margin_mm: i32,
    pub receipt_font_size_px: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            blind_count: false,
            denominations: DEFAULT_DENOMINATIONS.iter().map(|d| Decimal::from(*d)).collect(),
            require_mpesa_reference: true,
            receipt_paper_width_mm: 80,
            receipt_margin_mm: 4,
            receipt_font_size_px: 12,
        }
    }
}

/// The tenant's POS settings; the defaults when no row was ever written.
pub async fn load<C: ConnectionTrait>(conn: &C) -> Result<Settings> {
    let Some(stored) = row::Entity::find_by_id(true).one(conn).await? else {
        return Ok(Settings::default());
    };
    Ok(Settings {
        blind_count: stored.blind_count,
        denominations: parse_denominations(&stored.denominations)?,
        require_mpesa_reference: stored.require_mpesa_reference,
        receipt_paper_width_mm: stored.receipt_paper_width_mm,
        receipt_margin_mm: stored.receipt_margin_mm,
        receipt_font_size_px: stored.receipt_font_size_px,
    })
}

/// A stored denomination may be a JSON number (the migration's default) or
/// a string (what [`save`] writes, to keep decimal coins exact).
fn parse_denominations(value: &serde_json::Value) -> Result<Vec<Decimal>> {
    let Some(items) = value.as_array() else {
        return Err(Error::internal("pos_settings.denominations is not an array"));
    };
    items
        .iter()
        .map(|v| match v {
            serde_json::Value::String(s) => Decimal::from_str(s)
                .map_err(|e| Error::internal(format!("bad stored denomination {s:?}: {e}"))),
            other => Decimal::from_str(&other.to_string()).map_err(|e| {
                Error::internal(format!("bad stored denomination {other}: {e}"))
            }),
        })
        .collect()
}

/// Write the settings (insert or update — the row may never have existed).
pub async fn save<C: ConnectionTrait>(
    conn: &C,
    settings: &Settings,
    by: Option<Uuid>,
) -> Result<()> {
    let denominations = serde_json::Value::Array(
        settings
            .denominations
            .iter()
            .map(|d| serde_json::Value::String(d.normalize().to_string()))
            .collect(),
    );
    let now = chrono::Utc::now();
    match row::Entity::find_by_id(true).one(conn).await? {
        Some(existing) => {
            let mut active: row::ActiveModel = existing.into();
            active.blind_count = Set(settings.blind_count);
            active.denominations = Set(denominations);
            active.require_mpesa_reference = Set(settings.require_mpesa_reference);
            active.receipt_paper_width_mm = Set(settings.receipt_paper_width_mm);
            active.receipt_margin_mm = Set(settings.receipt_margin_mm);
            active.receipt_font_size_px = Set(settings.receipt_font_size_px);
            active.updated_at = Set(now);
            active.updated_by = Set(by);
            active.update(conn).await?;
        }
        None => {
            row::ActiveModel {
                id: Set(true),
                blind_count: Set(settings.blind_count),
                denominations: Set(denominations),
                require_mpesa_reference: Set(settings.require_mpesa_reference),
                receipt_paper_width_mm: Set(settings.receipt_paper_width_mm),
                receipt_margin_mm: Set(settings.receipt_margin_mm),
                receipt_font_size_px: Set(settings.receipt_font_size_px),
                updated_at: Set(now),
                updated_by: Set(by),
            }
            .insert(conn)
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateSettingsRequest {
    pub blind_count: bool,
    /// Positive amounts; the server sorts them largest-first and drops
    /// duplicates. Empty = no count-sheet helper.
    #[schema(value_type = Vec<String>)]
    pub denominations: Vec<String>,
    pub require_mpesa_reference: bool,
    pub receipt_paper_width_mm: i32,
    pub receipt_margin_mm: i32,
    pub receipt_font_size_px: i32,
}

pub(crate) fn routes() -> Router {
    Router::new().route("/pos/settings", get(get_settings).put(put_settings))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(get_settings, put_settings))]
struct ApiDoc;

/// Read by the till (it needs the count-sheet set and the blind flag) and
/// by whoever manages registers.
#[utoipa::path(get, path = "/pos/settings", tag = "pos",
    responses((status = 200, body = Settings)))]
async fn get_settings(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Settings>> {
    if !authz.is_granted(names::SELL).await? {
        authz.require(names::REGISTERS_MANAGE).await?;
    }
    load(&db).await.map(Json)
}

#[utoipa::path(put, path = "/pos/settings", tag = "pos",
    request_body = UpdateSettingsRequest,
    responses((status = 200, body = Settings)))]
async fn put_settings(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<Settings>> {
    authz.require(names::REGISTERS_MANAGE).await?;
    let mut denominations = Vec::with_capacity(req.denominations.len());
    for raw in &req.denominations {
        let d = Decimal::from_str(raw.trim())
            .map_err(|_| Error::Validation(format!("{raw:?} is not a denomination")))?;
        if d <= Decimal::ZERO {
            return Err(Error::Validation(
                "a denomination must be a positive amount".into(),
            ));
        }
        if !denominations.contains(&d) {
            denominations.push(d);
        }
    }
    denominations.sort_by(|a, b| b.cmp(a));
    // Paper bounds: narrower than 30mm is not a receipt, wider than 210mm
    // is a full page; the margin must leave some paper to print on.
    if !(30..=210).contains(&req.receipt_paper_width_mm) {
        return Err(Error::Validation(
            "receipt paper width must be between 30 and 210 mm".into(),
        ));
    }
    if req.receipt_margin_mm < 0 || req.receipt_margin_mm * 2 >= req.receipt_paper_width_mm {
        return Err(Error::Validation(
            "receipt margins must fit inside the paper".into(),
        ));
    }
    if !(6..=32).contains(&req.receipt_font_size_px) {
        return Err(Error::Validation(
            "receipt type size must be between 6 and 32 px".into(),
        ));
    }
    let settings = Settings {
        blind_count: req.blind_count,
        denominations,
        require_mpesa_reference: req.require_mpesa_reference,
        receipt_paper_width_mm: req.receipt_paper_width_mm,
        receipt_margin_mm: req.receipt_margin_mm,
        receipt_font_size_px: req.receipt_font_size_px,
    };
    save(&db, &settings, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!(
            "updated POS settings: blind count {}, {} denominations, M-Pesa code {}, {}mm paper",
            if settings.blind_count { "on" } else { "off" },
            settings.denominations.len(),
            if settings.require_mpesa_reference { "required" } else { "optional" },
            settings.receipt_paper_width_mm
        ))
        .await;
    Ok(Json(settings))
}
