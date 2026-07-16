//! Fiscal years and periods: the posting calendar.
//!
//! A fiscal year spans twelve consecutive monthly [`period`]s. A journal
//! entry may only be **posted** into an *open* period — closing a period
//! finalises that month, and locking makes the close permanent (a locked
//! period can never be reopened). Dates that fall outside every defined
//! period are unconstrained, so a tenant that never sets up a calendar can
//! still post freely (zero-config), while one that cares gets month-end
//! control.
//!
//! Rows live in the tenant's own database. All mutations are audited.

use crate::accounting::permissions::names;
use axum::extract::Path;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Datelike, NaiveDate};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryOrder, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A period's lifecycle: postings land only in `Open`; `Closed` finalises a
/// month (reversible); `Locked` is permanent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeriodStatus {
    Open,
    Closed,
    Locked,
}

impl PeriodStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PeriodStatus::Open => "open",
            PeriodStatus::Closed => "closed",
            PeriodStatus::Locked => "locked",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(PeriodStatus::Open),
            "closed" => Ok(PeriodStatus::Closed),
            "locked" => Ok(PeriodStatus::Locked),
            other => Err(Error::internal(format!("unknown period status {other:?}"))),
        }
    }
}

/// The fiscal year entity.
pub mod year {
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "accounting_fiscal_years")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub name: String,
        pub start_date: Date,
        pub end_date: Date,
        pub created_at: DateTimeUtc,
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// A monthly period within a fiscal year.
pub mod period {
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "accounting_fiscal_periods")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub fiscal_year_id: Uuid,
        pub period_number: i32,
        pub name: String,
        pub start_date: Date,
        pub end_date: Date,
        pub status: String,
        pub created_at: DateTimeUtc,
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Fiscal-calendar operations over one (tenant) connection.
pub struct FiscalService {
    db: DatabaseConnection,
}

impl FiscalService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Every fiscal year with its periods, newest first.
    pub async fn list(&self) -> Result<Vec<FiscalYearView>> {
        let years = year::Entity::find()
            .order_by_desc(year::Column::StartDate)
            .all(&self.db)
            .await?;
        let mut out = Vec::with_capacity(years.len());
        for y in years {
            let periods = period::Entity::find()
                .filter(period::Column::FiscalYearId.eq(y.id))
                .order_by_asc(period::Column::PeriodNumber)
                .all(&self.db)
                .await?;
            out.push(build_year_view(y, periods)?);
        }
        Ok(out)
    }

    /// Create a fiscal year (twelve monthly periods) starting on `start_date`,
    /// which must be the first day of a month. The name defaults to `FY<year>`
    /// (or `FY<y1>/<y2>` when the year crosses the calendar boundary).
    pub async fn create_year(
        &self,
        start_date: NaiveDate,
        name: Option<String>,
    ) -> Result<FiscalYearView> {
        if start_date.day() != 1 {
            return Err(Error::Validation(
                "a fiscal year must start on the first day of a month".into(),
            ));
        }
        let end_date = first_of_month_plus(start_date, 12)
            .pred_opt()
            .ok_or_else(|| Error::internal("fiscal year end date overflow"))?;

        // Reject a year that overlaps an existing period.
        let overlap = period::Entity::find()
            .filter(period::Column::StartDate.lte(end_date))
            .filter(period::Column::EndDate.gte(start_date))
            .count(&self.db)
            .await?;
        if overlap > 0 {
            return Err(Error::Conflict(
                "this range overlaps an existing fiscal period".into(),
            ));
        }

        let name = match name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => default_year_name(start_date, end_date),
        };

        let now = chrono::Utc::now();
        let year_id = Uuid::new_v4();
        let txn = self.db.begin().await?;
        year::ActiveModel {
            id: Set(year_id),
            name: Set(name),
            start_date: Set(start_date),
            end_date: Set(end_date),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await?;

        for i in 1..=12u32 {
            let p_start = first_of_month_plus(start_date, i - 1);
            let p_end = first_of_month_plus(start_date, i)
                .pred_opt()
                .ok_or_else(|| Error::internal("period end date overflow"))?;
            period::ActiveModel {
                id: Set(Uuid::new_v4()),
                fiscal_year_id: Set(year_id),
                period_number: Set(i as i32),
                name: Set(p_start.format("%b %Y").to_string()),
                start_date: Set(p_start),
                end_date: Set(p_end),
                status: Set(PeriodStatus::Open.as_str().to_string()),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        txn.commit().await?;

        self.find_year_view(year_id).await
    }

    /// Transition a period to `Closed` (from `Open`).
    pub async fn close_period(&self, id: Uuid) -> Result<FiscalYearView> {
        self.transition(id, PeriodStatus::Open, PeriodStatus::Closed)
            .await
    }

    /// Transition a period back to `Open` (from `Closed`; a `Locked` period
    /// cannot be reopened).
    pub async fn reopen_period(&self, id: Uuid) -> Result<FiscalYearView> {
        self.transition(id, PeriodStatus::Closed, PeriodStatus::Open)
            .await
    }

    /// Permanently lock a period (from `Closed`).
    pub async fn lock_period(&self, id: Uuid) -> Result<FiscalYearView> {
        self.transition(id, PeriodStatus::Closed, PeriodStatus::Locked)
            .await
    }

    async fn transition(
        &self,
        id: Uuid,
        from: PeriodStatus,
        to: PeriodStatus,
    ) -> Result<FiscalYearView> {
        let p = period::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("fiscal period {id}")))?;
        let current = PeriodStatus::parse(&p.status)?;
        if current != from {
            return Err(Error::Validation(format!(
                "cannot {} a period that is {}",
                match to {
                    PeriodStatus::Closed => "close",
                    PeriodStatus::Open => "reopen",
                    PeriodStatus::Locked => "lock",
                },
                current.as_str()
            )));
        }
        let year_id = p.fiscal_year_id;
        let mut active: period::ActiveModel = p.into();
        active.status = Set(to.as_str().to_string());
        active.updated_at = Set(chrono::Utc::now());
        active.update(&self.db).await?;
        self.find_year_view(year_id).await
    }

    async fn find_year_view(&self, year_id: Uuid) -> Result<FiscalYearView> {
        let y = year::Entity::find_by_id(year_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("fiscal year {year_id}")))?;
        let periods = period::Entity::find()
            .filter(period::Column::FiscalYearId.eq(year_id))
            .order_by_asc(period::Column::PeriodNumber)
            .all(&self.db)
            .await?;
        build_year_view(y, periods)
    }

    /// Ensure the current calendar year exists as a fiscal year, so a freshly
    /// seeded tenant can post out of the box. Idempotent: does nothing if any
    /// fiscal year already exists. Returns `true` if it created one.
    pub async fn ensure_current_year(&self) -> Result<bool> {
        let existing = year::Entity::find().count(&self.db).await?;
        if existing > 0 {
            return Ok(false);
        }
        let today = chrono::Utc::now().date_naive();
        let start = NaiveDate::from_ymd_opt(today.year(), 1, 1)
            .ok_or_else(|| Error::internal("invalid current year start"))?;
        self.create_year(start, None).await?;
        Ok(true)
    }
}

/// The status of the period covering `date`, or `None` if no period does.
pub async fn period_status_for_date<C: ConnectionTrait>(
    conn: &C,
    date: NaiveDate,
) -> Result<Option<PeriodStatus>> {
    let p = period::Entity::find()
        .filter(period::Column::StartDate.lte(date))
        .filter(period::Column::EndDate.gte(date))
        .one(conn)
        .await?;
    match p {
        Some(p) => Ok(Some(PeriodStatus::parse(&p.status)?)),
        None => Ok(None),
    }
}

/// Reject posting into a date whose period is closed or locked. A date with
/// no defined period is allowed (period control is opt-in).
pub async fn ensure_open_for_post<C: ConnectionTrait>(conn: &C, date: NaiveDate) -> Result<()> {
    if let Some(status) = period_status_for_date(conn, date).await? {
        if status != PeriodStatus::Open {
            return Err(Error::Validation(format!(
                "the accounting period for {date} is {}; reopen it before posting",
                status.as_str()
            )));
        }
    }
    Ok(())
}

/// The first day of the month `months` after a first-of-month `base`.
fn first_of_month_plus(base: NaiveDate, months: u32) -> NaiveDate {
    let total = base.month0() + months;
    let year = base.year() + (total / 12) as i32;
    let month = total % 12 + 1;
    NaiveDate::from_ymd_opt(year, month, 1).expect("valid first-of-month date")
}

fn default_year_name(start: NaiveDate, end: NaiveDate) -> String {
    if start.year() == end.year() {
        format!("FY{}", start.year())
    } else {
        format!("FY{}/{}", start.year(), end.year())
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
pub struct FiscalPeriodView {
    pub id: Uuid,
    pub period_number: i32,
    pub name: String,
    #[schema(value_type = String, format = Date)]
    pub start_date: NaiveDate,
    #[schema(value_type = String, format = Date)]
    pub end_date: NaiveDate,
    pub status: PeriodStatus,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct FiscalYearView {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = String, format = Date)]
    pub start_date: NaiveDate,
    #[schema(value_type = String, format = Date)]
    pub end_date: NaiveDate,
    /// Derived: `locked` if every period is locked, `closed` if none is open,
    /// otherwise `open`.
    pub status: PeriodStatus,
    pub periods: Vec<FiscalPeriodView>,
}

fn build_year_view(y: year::Model, periods: Vec<period::Model>) -> Result<FiscalYearView> {
    let mut views = Vec::with_capacity(periods.len());
    let mut any_open = false;
    let mut all_locked = !periods.is_empty();
    for p in periods {
        let status = PeriodStatus::parse(&p.status)?;
        any_open |= status == PeriodStatus::Open;
        all_locked &= status == PeriodStatus::Locked;
        views.push(FiscalPeriodView {
            id: p.id,
            period_number: p.period_number,
            name: p.name,
            start_date: p.start_date,
            end_date: p.end_date,
            status,
        });
    }
    let status = if all_locked {
        PeriodStatus::Locked
    } else if any_open {
        PeriodStatus::Open
    } else {
        PeriodStatus::Closed
    };
    Ok(FiscalYearView {
        id: y.id,
        name: y.name,
        start_date: y.start_date,
        end_date: y.end_date,
        status,
        periods: views,
    })
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(super) fn routes() -> Router {
    Router::new()
        .route(
            "/accounting/fiscal-years",
            get(list_fiscal_years).post(create_fiscal_year),
        )
        .route("/accounting/periods/{id}/close", post(close_period))
        .route("/accounting/periods/{id}/reopen", post(reopen_period))
        .route("/accounting/periods/{id}/lock", post(lock_period))
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_fiscal_years,
    create_fiscal_year,
    close_period,
    reopen_period,
    lock_period
))]
struct ApiDoc;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateFiscalYearRequest {
    /// Optional display name; defaults to `FY<year>`.
    pub name: Option<String>,
    /// First day of the year; must be the first day of a month.
    #[schema(value_type = String, format = Date)]
    pub start_date: NaiveDate,
}

#[utoipa::path(get, path = "/accounting/fiscal-years", tag = "accounting",
    responses((status = 200, body = Vec<FiscalYearView>)))]
async fn list_fiscal_years(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<Vec<FiscalYearView>>> {
    authz.require(names::FISCAL_YEARS_VIEW).await?;
    FiscalService::new(db).list().await.map(Json)
}

#[utoipa::path(post, path = "/accounting/fiscal-years", tag = "accounting",
    request_body = CreateFiscalYearRequest,
    responses((status = 200, body = FiscalYearView)))]
async fn create_fiscal_year(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateFiscalYearRequest>,
) -> Result<Json<FiscalYearView>> {
    authz.require(names::FISCAL_YEARS_MANAGE).await?;
    let year = FiscalService::new(db)
        .create_year(req.start_date, req.name)
        .await?;
    audit
        .0
        .created("accounting.fiscal_year", year.id, &year)
        .await;
    Ok(Json(year))
}

#[utoipa::path(post, path = "/accounting/periods/{id}/close", tag = "accounting",
    params(("id" = Uuid, Path, description = "Period id")),
    responses((status = 200, body = FiscalYearView)))]
async fn close_period(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<FiscalYearView>> {
    authz.require(names::FISCAL_YEARS_MANAGE).await?;
    let year = FiscalService::new(db).close_period(id).await?;
    audit
        .0
        .event(format!("closed accounting period {id}"))
        .await;
    Ok(Json(year))
}

#[utoipa::path(post, path = "/accounting/periods/{id}/reopen", tag = "accounting",
    params(("id" = Uuid, Path, description = "Period id")),
    responses((status = 200, body = FiscalYearView)))]
async fn reopen_period(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<FiscalYearView>> {
    authz.require(names::FISCAL_YEARS_MANAGE).await?;
    let year = FiscalService::new(db).reopen_period(id).await?;
    audit
        .0
        .event(format!("reopened accounting period {id}"))
        .await;
    Ok(Json(year))
}

#[utoipa::path(post, path = "/accounting/periods/{id}/lock", tag = "accounting",
    params(("id" = Uuid, Path, description = "Period id")),
    responses((status = 200, body = FiscalYearView)))]
async fn lock_period(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<FiscalYearView>> {
    authz.require(names::FISCAL_YEARS_MANAGE).await?;
    let year = FiscalService::new(db).lock_period(id).await?;
    audit
        .0
        .event(format!("locked accounting period {id}"))
        .await;
    Ok(Json(year))
}
