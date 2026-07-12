//! HTTP surface for the reporting engine: the axum routes and handlers.
//! Kept separate from the engine, document model and renderers in the
//! parent module so the transport layer and the logic evolve apart.

use super::{
    RenderCx, ReportFormat, ReportInfo, ReportJob, ReportOutput, ReportSettings, ReportTables,
    Reporting,
};
use crate::auth::authz::Authz;
use crate::auth::permission;
use crate::error::{Error, Result};
use crate::jobs::Jobs;
use crate::tenancy::TenantRef;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

/// The reporting routes, merged into the app by the kernel:
/// - `GET /reports` — the report catalogue
/// - `GET|PUT /reports/settings` — the tenant's report preferences
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

/// The recent background job history for the tenant.
async fn list_report_jobs(
    Extension(reporting): Extension<Reporting>,
    _authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
) -> Result<Json<Vec<ReportJob>>> {
    Ok(Json(reporting.jobs(db.as_ref().map(|e| &e.0), 50).await?))
}

/// One background job's status — polled by the viewer until `completed`.
async fn get_report_job(
    Extension(reporting): Extension<Reporting>,
    _authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<ReportJob>> {
    Ok(Json(reporting.job(db.as_ref().map(|e| &e.0), id).await?))
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
