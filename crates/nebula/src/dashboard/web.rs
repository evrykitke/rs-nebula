//! HTTP surface for the dashboard engine: the axum routes and handlers.
//! Kept separate from the registry and payload types in the parent
//! module so the transport layer and the logic evolve apart.
//!
//! Everything here is per-caller: layouts are the authenticated user's
//! own, and every path filters or refuses by each widget's permission.

use super::{
    DashboardView, Dashboards, PlacedWidget, PlacedWidgetView, UpdateDashboardRequest, WidgetCx,
    WidgetData, WidgetDefinition, WidgetInfo, GRID_COLUMNS, MAX_WIDGETS,
};
use crate::auth::authz::Authz;
use crate::error::{Error, Result};
use crate::tenancy::TenantRef;
use axum::extract::Path;
use axum::routing::get;
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use std::sync::Arc;

/// The dashboard routes, merged into the app by the kernel:
/// - `GET /dashboards/{dashboard}` — the caller's layout (or the default)
/// - `PUT /dashboards/{dashboard}` — save the caller's arrangement
/// - `DELETE /dashboards/{dashboard}` — back to the default layout
/// - `GET /dashboards/{dashboard}/widgets` — the permitted catalogue
/// - `GET /dashboards/{dashboard}/widgets/{name}/data` — one widget's
///   data, loaded lazily as the tile becomes visible
pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/dashboards/{dashboard}",
            get(get_dashboard).put(put_dashboard).delete(reset_dashboard),
        )
        .route("/dashboards/{dashboard}/widgets", get(dashboard_widgets))
        .route(
            "/dashboards/{dashboard}/widgets/{name}/data",
            get(widget_data),
        )
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

/// The dashboard endpoints' OpenAPI contribution — the source client
/// generators (NSwag) build the `dashboard` service proxy from.
#[derive(utoipa::OpenApi)]
#[openapi(paths(
    get_dashboard,
    put_dashboard,
    reset_dashboard,
    dashboard_widgets,
    widget_data
))]
struct ApiDoc;

/// 404 for a canvas no module declared widgets for — a typo'd URL, not
/// an empty dashboard.
fn require_dashboard(dashboards: &Dashboards, name: &str) -> Result<()> {
    if dashboards.contains_dashboard(name) {
        Ok(())
    } else {
        Err(Error::NotFound(format!("dashboard {name:?}")))
    }
}

/// Resolve a stored (or default) layout into what this caller renders:
/// unknown widgets dropped (a module retired one), foreign-dashboard
/// widgets dropped (defensive), unpermitted widgets hidden without
/// touching the saved arrangement — regaining the permission brings the
/// tile back where it was.
async fn resolve(
    dashboards: &Dashboards,
    authz: &Authz,
    dashboard: &str,
    layout: Vec<PlacedWidget>,
) -> Result<Vec<PlacedWidgetView>> {
    let mut views = Vec::with_capacity(layout.len());
    for placed in layout {
        let Some(def) = dashboards.get(&placed.widget) else {
            continue;
        };
        if def.dashboard() != dashboard {
            continue;
        }
        if !permitted(authz, def.as_ref()).await? {
            continue;
        }
        views.push(PlacedWidgetView {
            name: def.name().to_string(),
            title: def.title().to_string(),
            description: def.description().to_string(),
            kind: def.kind(),
            span: placed.span.clamp(1, GRID_COLUMNS as i32),
        });
    }
    Ok(views)
}

async fn permitted(authz: &Authz, def: &dyn WidgetDefinition) -> Result<bool> {
    match def.permission() {
        Some(required) => authz.is_granted(required).await,
        None => Ok(true),
    }
}

/// The caller's saved layout when they have one and a database is
/// configured; the dashboard's default otherwise.
async fn stored_or_default(
    dashboards: &Dashboards,
    db: Option<&DatabaseConnection>,
    user_id: uuid::Uuid,
    dashboard: &str,
) -> Result<(bool, Vec<PlacedWidget>)> {
    if let Some(db) = db {
        if let Some(layout) = dashboards.layout(db, user_id, dashboard).await? {
            return Ok((true, layout));
        }
    }
    Ok((false, dashboards.default_layout(dashboard)))
}

#[utoipa::path(get, path = "/dashboards/{dashboard}", tag = "dashboard",
    params(("dashboard" = String, Path, description = "Dashboard name")),
    responses((status = 200, body = DashboardView)))]
async fn get_dashboard(
    Extension(dashboards): Extension<Dashboards>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Path(dashboard): Path<String>,
) -> Result<Json<DashboardView>> {
    require_dashboard(&dashboards, &dashboard)?;
    let (customized, layout) = stored_or_default(
        &dashboards,
        db.as_ref().map(|e| &e.0),
        authz.user.id,
        &dashboard,
    )
    .await?;
    let widgets = resolve(&dashboards, &authz, &dashboard, layout).await?;
    Ok(Json(DashboardView {
        dashboard,
        max_widgets: MAX_WIDGETS as i32,
        customized,
        widgets,
    }))
}

#[utoipa::path(put, path = "/dashboards/{dashboard}", tag = "dashboard",
    params(("dashboard" = String, Path, description = "Dashboard name")),
    request_body = UpdateDashboardRequest,
    responses((status = 200, body = DashboardView)))]
async fn put_dashboard(
    Extension(dashboards): Extension<Dashboards>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Path(dashboard): Path<String>,
    Json(req): Json<UpdateDashboardRequest>,
) -> Result<Json<DashboardView>> {
    require_dashboard(&dashboards, &dashboard)?;
    let db = db
        .map(|e| e.0)
        .ok_or_else(|| Error::internal("dashboard layouts require a database connection"))?;

    dashboards.validate_layout(&dashboard, &req.widgets)?;
    for placed in &req.widgets {
        let def = dashboards
            .get(&placed.widget)
            .expect("validate_layout checked every widget exists");
        // Saving a widget you cannot see would create a tile that read
        // filters out anyway — refuse it so the client learns now.
        if !permitted(&authz, def.as_ref()).await? {
            return Err(Error::Forbidden);
        }
    }

    dashboards
        .save_layout(&db, authz.user.id, &dashboard, &req.widgets)
        .await?;
    let widgets = resolve(&dashboards, &authz, &dashboard, req.widgets).await?;
    Ok(Json(DashboardView {
        dashboard,
        max_widgets: MAX_WIDGETS as i32,
        customized: true,
        widgets,
    }))
}

#[utoipa::path(delete, path = "/dashboards/{dashboard}", tag = "dashboard",
    params(("dashboard" = String, Path, description = "Dashboard name")),
    responses((status = 200, body = DashboardView)))]
async fn reset_dashboard(
    Extension(dashboards): Extension<Dashboards>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    Path(dashboard): Path<String>,
) -> Result<Json<DashboardView>> {
    require_dashboard(&dashboards, &dashboard)?;
    if let Some(Extension(db)) = db {
        dashboards.reset_layout(&db, authz.user.id, &dashboard).await?;
    }
    let layout = dashboards.default_layout(&dashboard);
    let widgets = resolve(&dashboards, &authz, &dashboard, layout).await?;
    Ok(Json(DashboardView {
        dashboard,
        max_widgets: MAX_WIDGETS as i32,
        customized: false,
        widgets,
    }))
}

#[utoipa::path(get, path = "/dashboards/{dashboard}/widgets", tag = "dashboard",
    params(("dashboard" = String, Path, description = "Dashboard name")),
    responses((status = 200, body = Vec<WidgetInfo>)))]
async fn dashboard_widgets(
    Extension(dashboards): Extension<Dashboards>,
    authz: Authz,
    Path(dashboard): Path<String>,
) -> Result<Json<Vec<WidgetInfo>>> {
    require_dashboard(&dashboards, &dashboard)?;
    let mut infos = Vec::new();
    for def in dashboards.widgets_of(&dashboard) {
        if !permitted(&authz, def.as_ref()).await? {
            continue;
        }
        infos.push(WidgetInfo {
            name: def.name().to_string(),
            dashboard: def.dashboard().to_string(),
            title: def.title().to_string(),
            description: def.description().to_string(),
            kind: def.kind(),
            default_span: def.default_span() as i32,
        });
    }
    Ok(Json(infos))
}

#[utoipa::path(get, path = "/dashboards/{dashboard}/widgets/{name}/data", tag = "dashboard",
    params(
        ("dashboard" = String, Path, description = "Dashboard name"),
        ("name" = String, Path, description = "Widget name")
    ),
    responses((status = 200, body = WidgetData)))]
async fn widget_data(
    Extension(dashboards): Extension<Dashboards>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    tenant: Option<Extension<TenantRef>>,
    Path((dashboard, name)): Path<(String, String)>,
) -> Result<Json<WidgetData>> {
    require_dashboard(&dashboards, &dashboard)?;
    let def: Arc<dyn WidgetDefinition> = dashboards
        .get(&name)
        .filter(|d| d.dashboard() == dashboard)
        .cloned()
        .ok_or_else(|| Error::NotFound(format!("widget {name:?}")))?;
    if let Some(required) = def.permission() {
        authz.require(required).await?;
    }
    let cx = WidgetCx {
        db: db.as_ref().map(|e| &e.0),
        tenant: tenant.as_ref().map(|e| &e.0),
        user_id: authz.user.id,
    };
    Ok(Json(def.load(&cx).await?))
}
