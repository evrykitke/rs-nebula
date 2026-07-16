//! HTTP surface for the reporting engine: the axum routes and handlers.
//! Kept separate from the engine, document model and renderers in the
//! parent module so the transport layer and the logic evolve apart.

use super::{
    Column, Orientation, RenderCx, Report, ReportFormat, ReportInfo, ReportJob, ReportOutput,
    ReportSettings, ReportTables, Reporting, Table,
};
use crate::auth::authz::Authz;
use crate::auth::permission;
use crate::error::{Error, Result};
use crate::jobs::Jobs;
use crate::tenancy::TenantRef;
use axum::extract::{DefaultBodyLimit, Path, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

/// The reporting routes, merged into the app by the kernel:
/// - `GET /reports` — the report catalogue
/// - `GET|PUT /reports/settings` — the tenant's report preferences
/// - `POST /reports/list-export?output=pdf` — render a caller-supplied list
/// - `GET /reports/{name}?format=modern&output=pdf` — render a report
/// - `GET /reports/{name}/preview` — themed SVG preview pages
/// - `GET /reports/{name}/table` — the interactive datatable payload
/// - `POST /reports/{name}/jobs` — queue a background render
/// - `GET /reports/jobs`, `GET /reports/jobs/{id}`,
///   `GET /reports/jobs/{id}/download` — job history, status and artifact
pub(crate) fn routes() -> Router {
    Router::new()
        .route("/reports", get(list_reports))
        .route("/reports/settings", get(get_settings).put(put_settings))
        .route(
            "/reports/list-export",
            // A list export carries its rows in the body, so it needs far more
            // room than axum's 2 MB default — which a wide list of a few
            // thousand rows reaches, and would fail as an opaque 413.
            post(export_list).layer(DefaultBodyLimit::max(EXPORT_BODY_LIMIT)),
        )
        .route("/reports/{name}", get(render_report))
        .route("/reports/{name}/preview", get(preview_report))
        .route("/reports/{name}/table", get(table_report))
        .route("/reports/{name}/jobs", post(enqueue_report_job))
        .route("/reports/jobs", get(list_report_jobs))
        .route("/reports/jobs/{id}", get(get_report_job))
        .route("/reports/jobs/{id}/download", get(download_report_job))
}

#[derive(Debug, Deserialize)]
struct RenderParams {
    format: Option<String>,
    output: Option<String>,
}

/// Parse an optional `format` query value into a [`ReportFormat`].
fn parse_format(value: Option<&str>) -> Result<Option<ReportFormat>> {
    match value {
        Some(s) => Ok(Some(ReportFormat::parse(s).ok_or_else(|| {
            Error::Validation(format!("unknown report format {s:?}"))
        })?)),
        None => Ok(None),
    }
}

/// Parse an optional `output` query value, defaulting to the report's PDF.
fn parse_output(value: Option<&str>) -> Result<ReportOutput> {
    match value {
        Some(s) => ReportOutput::parse(s)
            .ok_or_else(|| Error::Validation(format!("unknown report output {s:?}"))),
        None => Ok(ReportOutput::default()),
    }
}

/// The themed in-app preview: the report's pages as SVG, for the viewer to
/// render inside the app's own chrome (instead of the browser's native PDF
/// viewer). One string per page.
#[derive(Debug, Serialize)]
struct ReportPreview {
    pages: Vec<String>,
}

async fn list_reports(
    Extension(reporting): Extension<Reporting>,
    _authz: Authz,
) -> Result<Json<Vec<ReportInfo>>> {
    Ok(Json(reporting.catalogue()))
}

async fn get_settings(
    Extension(reporting): Extension<Reporting>,
    _authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
) -> Result<Json<ReportSettings>> {
    Ok(Json(reporting.settings(db.as_ref().map(|e| &e.0)).await))
}

async fn put_settings(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Json(settings): Json<ReportSettings>,
) -> Result<Json<ReportSettings>> {
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let db = db
        .map(|e| e.0)
        .ok_or_else(|| Error::internal("report settings require a database connection"))?;
    reporting.save_settings(&db, &settings).await?;
    Ok(Json(reporting.settings(Some(&db)).await))
}

async fn render_report(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    tenant: Option<Extension<TenantRef>>,
    Path(name): Path<String>,
    Query(params): Query<RenderParams>,
) -> Result<Response> {
    // Rendering requires an authenticated tenant user; reports that declare
    // a permission require that too.
    if let Some(required) = reporting.required_permission(&name) {
        authz.require(required).await?;
    }

    let format = parse_format(params.format.as_deref())?;
    let output = parse_output(params.output.as_deref())?;

    let cx = RenderCx {
        db: db.map(|e| e.0),
        tenant: tenant.map(|e| e.0),
    };
    let rendered = reporting.render(&cx, &name, format, output).await?;

    let disposition = format!("inline; filename=\"{name}.{}\"", rendered.extension);
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, rendered.content_type.to_string()),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        rendered.bytes,
    )
        .into_response())
}

/// The most rows one export may carry. A list far past this is a data dump,
/// not a document someone reads — it belongs in Excel, and holding a request
/// (and the renderer) that long is worse than saying no.
const MAX_EXPORT_ROWS: usize = 20_000;

/// Body room for an export: enough for [`MAX_EXPORT_ROWS`] of a wide list,
/// so the row cap is what rejects an oversized export (with an explanation)
/// rather than the transport.
const EXPORT_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// A list handed over for rendering: the table the user is looking at, with
/// its columns already resolved (hidden ones dropped) and its cells already
/// formatted by the client that displayed them.
#[derive(Debug, Deserialize)]
struct ListExport {
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    orientation: Orientation,
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    #[serde(default)]
    totals: Option<Vec<String>>,
}

impl ListExport {
    /// Validate the payload and turn it into a one-table [`Report`]. The
    /// table takes no title of its own: the report's title heads the page,
    /// and repeating it directly beneath is the duplication this endpoint
    /// exists to avoid.
    fn into_report(self) -> Result<Report> {
        if self.title.trim().is_empty() {
            return Err(Error::Validation("an export needs a title".into()));
        }
        if self.columns.is_empty() {
            return Err(Error::Validation("an export needs at least one column".into()));
        }
        if self.rows.len() > MAX_EXPORT_ROWS {
            return Err(Error::Validation(format!(
                "an export carries at most {MAX_EXPORT_ROWS} rows; this one has {}",
                self.rows.len()
            )));
        }
        let width = self.columns.len();
        if let Some((n, row)) = self.rows.iter().enumerate().find(|(_, r)| r.len() != width) {
            return Err(Error::Validation(format!(
                "row {n} has {} cells but there are {width} columns",
                row.len()
            )));
        }
        if let Some(totals) = self.totals.as_ref().filter(|t| t.len() != width) {
            return Err(Error::Validation(format!(
                "the totals row has {} cells but there are {width} columns",
                totals.len()
            )));
        }

        let table = Table {
            title: None,
            columns: self.columns,
            rows: self.rows,
            totals: self.totals,
        };
        let mut report = Report::new(self.title).orientation(self.orientation);
        if let Some(sub) = self.subtitle.filter(|s| !s.trim().is_empty()) {
            report = report.subtitle(sub);
        }
        Ok(report.with(table.into_widget()))
    }
}

/// A filename-safe slug of a title, e.g. "Purchase Orders" → "purchase-orders".
/// Falls back to `export` when a title has nothing slug-able in it.
fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let slug = out.trim_matches('-');
    if slug.is_empty() { "export".to_string() } else { slug.to_string() }
}

/// Render a list the caller supplies — the datatable "Export PDF" action.
///
/// Unlike `/reports/{name}`, no report definition backs this: the client
/// sends the columns and rows it is showing, and the engine dresses them in
/// the tenant's letterhead. That keeps one definition of a list's columns
/// (the client's table config) instead of a second copy on the server that
/// would drift from the screen.
///
/// Any authenticated tenant user may call it: the rows are data the caller
/// already holds, so this discloses nothing new — it only re-renders it.
async fn export_list(
    Extension(reporting): Extension<Reporting>,
    _authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    tenant: Option<Extension<TenantRef>>,
    Query(params): Query<RenderParams>,
    Json(payload): Json<ListExport>,
) -> Result<Response> {
    let format = parse_format(params.format.as_deref())?;
    let output = parse_output(params.output.as_deref())?;
    let name = slug(&payload.title);
    let report = payload.into_report()?;

    let cx = RenderCx {
        db: db.map(|e| e.0),
        tenant: tenant.map(|e| e.0),
    };
    let rendered = reporting.render_ad_hoc(&cx, report, format, output).await?;

    let disposition = format!("attachment; filename=\"{name}.{}\"", rendered.extension);
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, rendered.content_type.to_string()),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        rendered.bytes,
    )
        .into_response())
}

async fn preview_report(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    tenant: Option<Extension<TenantRef>>,
    Path(name): Path<String>,
    Query(params): Query<RenderParams>,
) -> Result<Json<ReportPreview>> {
    if let Some(required) = reporting.required_permission(&name) {
        authz.require(required).await?;
    }

    let format = parse_format(params.format.as_deref())?;
    let cx = RenderCx {
        db: db.map(|e| e.0),
        tenant: tenant.map(|e| e.0),
    };
    let pages = reporting.preview(&cx, &name, format).await?;
    Ok(Json(ReportPreview { pages }))
}

async fn table_report(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    tenant: Option<Extension<TenantRef>>,
    Path(name): Path<String>,
    Query(params): Query<RenderParams>,
) -> Result<Json<ReportTables>> {
    if let Some(required) = reporting.required_permission(&name) {
        authz.require(required).await?;
    }

    let format = parse_format(params.format.as_deref())?;
    let cx = RenderCx {
        db: db.map(|e| e.0),
        tenant: tenant.map(|e| e.0),
    };
    Ok(Json(reporting.datatables(&cx, &name, format).await?))
}

/// Queue a report to render in the background — for data-heavy reports that
/// would otherwise tie up the request. Answers the created [`ReportJob`];
/// the client polls `GET /reports/jobs/{id}` and downloads once completed.
/// Needs `jobs.enabled`.
async fn enqueue_report_job(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    audit: crate::audit::Audit,
    jobs: Option<Extension<Jobs>>,
    db: Option<Extension<DatabaseConnection>>,
    tenant: Option<Extension<TenantRef>>,
    Path(name): Path<String>,
    Query(params): Query<RenderParams>,
) -> Result<Json<ReportJob>> {
    if let Some(required) = reporting.required_permission(&name) {
        authz.require(required).await?;
    }
    let Some(Extension(jobs)) = jobs else {
        return Err(Error::Validation(
            "background jobs are not enabled on this deployment".into(),
        ));
    };

    let format = parse_format(params.format.as_deref())?;
    let output = parse_output(params.output.as_deref())?;

    let cx = RenderCx {
        db: db.map(|e| e.0),
        tenant: tenant.map(|e| e.0),
    };
    let requested_by = Some((authz.user.id, authz.user.user_name.clone()));
    let job = reporting
        .enqueue_job(&cx, &jobs, &name, format, output, requested_by)
        .await?;
    audit
        .0
        .event(format!("{} queued the report {name:?}", authz.user.user_name))
        .await;
    Ok(Json(job))
}

/// The recent background job history for the tenant. Jobs for reports
/// the caller may not view are filtered out — the history must not show
/// more than `download` would serve (who ran what, failure details).
async fn list_report_jobs(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
) -> Result<Json<Vec<ReportJob>>> {
    let jobs = reporting.jobs(db.as_ref().map(|e| &e.0), 50).await?;
    let mut visible = Vec::with_capacity(jobs.len());
    for job in jobs {
        match reporting.required_permission(&job.report) {
            Some(required) if !authz.is_granted(required).await? => {}
            _ => visible.push(job),
        }
    }
    Ok(Json(visible))
}

/// One background job's status — polled by the viewer until `completed`.
/// Guarded by the report's permission, like `download`.
async fn get_report_job(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<ReportJob>> {
    let job = reporting.job(db.as_ref().map(|e| &e.0), id).await?;
    if let Some(required) = reporting.required_permission(&job.report) {
        authz.require(required).await?;
    }
    Ok(Json(job))
}

/// Download a completed job's stored artifact. Served through the app (with
/// the report's permission enforced) rather than as a raw public URL.
async fn download_report_job(
    Extension(reporting): Extension<Reporting>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Response> {
    let db = db.map(|e| e.0);
    let job = reporting.job(db.as_ref(), id).await?;
    if let Some(required) = reporting.required_permission(&job.report) {
        authz.require(required).await?;
    }

    let (job, bytes) = reporting.artifact(db.as_ref(), id).await?;
    let filename = job
        .file_name
        .unwrap_or_else(|| format!("{}.bin", job.report));
    let content_type = job
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let disposition = format!("attachment; filename=\"{filename}\"");
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}
