//! The dashboard engine: a framework primitive for the landing page of
//! every app — a canvas of live widgets the user arranges.
//!
//! Dashboards mirror how [`crate::reporting`] works, because the shape of
//! the problem is the same: modules *declare* what they can show, the
//! kernel builds one registry, and a small framework HTTP surface serves
//! every declaration uniformly.
//!
//! - A **widget** is one tile of live data — a stat card, a chart, a
//!   table, a list, a set of progress bars. Modules declare them in
//!   `configure` (`ctx.declare_widget(...)`), each naming the dashboard
//!   it belongs to and the permission it requires.
//! - A **dashboard** is a named canvas ("workspace", "accounting",
//!   "pos", …) that exists because widgets were declared for it. A user
//!   composes their own layout from the catalogue — which widgets, in
//!   what order, at what width — capped at [`MAX_WIDGETS`]; the layout
//!   is remembered per user per dashboard in the tenant's database. No
//!   saved layout means the *default* layout: the widgets that declared
//!   a default position.
//! - **Data is lazy.** The layout endpoint returns placement only; the
//!   client fetches each widget's data through its own endpoint as the
//!   tile scrolls into view, so opening a dashboard costs placement +
//!   the visible tiles, not every query the canvas could ever run.
//!
//! Permissions are enforced in every direction: the catalogue only
//! offers what the caller may see, saved layouts are filtered on read
//! (a revoked permission hides the tile without touching the saved
//! arrangement), and the data endpoint checks again before running the
//! widget's queries.

use crate::error::{Error, Result};
use crate::tenancy::TenantRef;
use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

mod web;
pub(crate) use web::api;
pub(crate) use web::routes;

/// The most widgets one canvas holds. A dashboard is a glance, not a
/// report: past this it stops being readable, and every extra tile is
/// another set of queries per visit.
pub const MAX_WIDGETS: usize = 12;

/// The grid a dashboard lays out on. Spans are columns of this grid.
pub const GRID_COLUMNS: u8 = 12;

// ---------------------------------------------------------------------------
// Widget kinds and data payloads
// ---------------------------------------------------------------------------

/// What a widget looks like — how the client renders its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    /// One number with a caption and an optional trend — the KPI card.
    Stat,
    /// Labels and series drawn as one of the [`ChartType`]s.
    Chart,
    /// A small data table.
    Table,
    /// A titled list of records (recent activity, top N).
    List,
    /// One or more labelled progress bars (utilization, goals).
    Progress,
}

impl WidgetKind {
    /// The grid width a widget of this kind reads best at, used when a
    /// definition does not choose its own.
    pub fn natural_span(self) -> u8 {
        match self {
            WidgetKind::Stat => 3,
            WidgetKind::Chart => 6,
            WidgetKind::Table => 6,
            WidgetKind::List => 4,
            WidgetKind::Progress => 4,
        }
    }
}

/// How a [`WidgetKind::Chart`] widget draws its series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChartType {
    Line,
    /// Line with the area beneath filled.
    Area,
    Bar,
    /// Bars stacked per category.
    StackedBar,
    Pie,
    /// Pie with a hollow centre.
    Donut,
}

/// A trend hint the client colours (up = good is the client's call, not
/// the data's — a rising expense is still `Up`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    Up,
    Down,
    Flat,
}

/// A stat card: the value as display text (the widget owns its own
/// formatting — money, counts and percentages all render differently),
/// with an optional caption and period-over-period delta.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StatData {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// A short comparison line, e.g. "+12% vs last month".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<TrendDirection>,
}

/// One named series of values aligned to the chart's labels.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SeriesData {
    pub name: String,
    pub values: Vec<f64>,
}

/// A chart payload: category labels plus one or more aligned series.
/// Pie/donut charts read the first series as the slice values.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ChartData {
    pub chart: ChartType,
    pub labels: Vec<String>,
    pub series: Vec<SeriesData>,
    /// A unit hint for axis/tooltip formatting, e.g. "KES".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TableColumnData {
    pub label: String,
    /// Numeric columns right-align.
    #[serde(default)]
    pub numeric: bool,
}

/// A small table: string-rendered cells, like the reporting datatables.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TableData {
    pub columns: Vec<TableColumnData>,
    pub rows: Vec<Vec<String>>,
    /// What to say when there are no rows, e.g. "No sessions today."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_text: Option<String>,
}

/// One entry of a [`ListData`] widget.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ListItemData {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// The right-aligned figure, e.g. an amount or a count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<TrendDirection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ListData {
    pub items: Vec<ListItemData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_text: Option<String>,
}

/// One bar of a [`ProgressData`] widget. `value` is a fraction 0.0..=1.0.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProgressItemData {
    pub label: String,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProgressData {
    pub items: Vec<ProgressItemData>,
}

/// A widget's loaded data. Deliberately flat rather than a tagged union:
/// `kind` says which section is filled, and generated clients get plain
/// optional fields instead of a discriminated type they may mishandle.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WidgetData {
    pub kind: WidgetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<StatData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<TableData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<ListData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressData>,
}

impl WidgetData {
    pub fn stat(data: StatData) -> Self {
        Self { kind: WidgetKind::Stat, stat: Some(data), chart: None, table: None, list: None, progress: None }
    }
    pub fn chart(data: ChartData) -> Self {
        Self { kind: WidgetKind::Chart, stat: None, chart: Some(data), table: None, list: None, progress: None }
    }
    pub fn table(data: TableData) -> Self {
        Self { kind: WidgetKind::Table, stat: None, chart: None, table: Some(data), list: None, progress: None }
    }
    pub fn list(data: ListData) -> Self {
        Self { kind: WidgetKind::List, stat: None, chart: None, table: None, list: Some(data), progress: None }
    }
    pub fn progress(data: ProgressData) -> Self {
        Self { kind: WidgetKind::Progress, stat: None, chart: None, table: None, list: None, progress: Some(data) }
    }
}

// ---------------------------------------------------------------------------
// Widget definitions
// ---------------------------------------------------------------------------

/// What a widget is handed when it loads: the request's (tenant-swapped)
/// database connection, the current tenant, and who is looking — some
/// widgets are personal ("my open sessions").
pub struct WidgetCx<'a> {
    pub db: Option<&'a DatabaseConnection>,
    pub tenant: Option<&'a TenantRef>,
    pub user_id: Uuid,
}

impl WidgetCx<'_> {
    /// The request database, for widgets that cannot function without one.
    pub fn require_db(&self) -> Result<&DatabaseConnection> {
        self.db
            .ok_or_else(|| Error::internal("this widget requires a database connection"))
    }
}

/// A widget a module declares: its identity, where it lives, what it
/// needs, and how to load its data. The declaration is everything the
/// catalogue shows; [`WidgetDefinition::load`] runs only when a client
/// asks for the widget's data.
#[async_trait]
pub trait WidgetDefinition: Send + Sync {
    /// Unique name across the whole application; also the URL segment.
    /// Prefix with the module for hygiene, e.g. `pos-takings-today`.
    fn name(&self) -> &'static str;
    /// The dashboard (canvas) this widget belongs to, e.g. "workspace",
    /// "accounting", "inventory", "procurement", "sales", "pos".
    fn dashboard(&self) -> &'static str;
    /// Human title shown on the tile and in the customize catalogue.
    fn title(&self) -> &'static str;
    /// One line for the catalogue: what the widget shows.
    fn description(&self) -> &'static str;
    fn kind(&self) -> WidgetKind;
    /// The permission a caller needs to see this widget (catalogue, layout
    /// and data alike). `None` means any user of the tenant.
    fn permission(&self) -> Option<&'static str> {
        None
    }
    /// The tile's default width in grid columns (1..=[`GRID_COLUMNS`]).
    fn default_span(&self) -> u8 {
        self.kind().natural_span()
    }
    /// `Some(n)` puts this widget on the dashboard's *default* layout —
    /// what a user sees before they customize — sorted by `n`. `None`
    /// keeps it catalogue-only.
    fn default_position(&self) -> Option<u8> {
        None
    }
    /// Run the widget's queries and shape its payload. The payload's
    /// `kind` should match [`WidgetDefinition::kind`] — the client lays
    /// the tile out before the data arrives.
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData>;
}

/// A widget's public metadata for the customize catalogue.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WidgetInfo {
    pub name: String,
    pub dashboard: String,
    pub title: String,
    pub description: String,
    pub kind: WidgetKind,
    pub default_span: i32,
}

// ---------------------------------------------------------------------------
// Layouts
// ---------------------------------------------------------------------------

/// One placed widget in a saved layout: which widget, at what width.
/// Order in the vector is order on the canvas — the grid flows them.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlacedWidget {
    pub widget: String,
    /// Width in grid columns, 1..=12.
    pub span: i32,
}

/// A placed widget resolved against its definition — what the canvas
/// renders before any data arrives.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PlacedWidgetView {
    pub name: String,
    pub title: String,
    pub description: String,
    pub kind: WidgetKind,
    pub span: i32,
}

/// A dashboard as the caller sees it: their layout (or the default),
/// already filtered to the widgets they may see.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DashboardView {
    pub dashboard: String,
    /// The canvas cap — the client disables "add" at this count.
    pub max_widgets: i32,
    /// Whether this is the caller's own saved arrangement (true) or the
    /// dashboard's default (false).
    pub customized: bool,
    pub widgets: Vec<PlacedWidgetView>,
}

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct UpdateDashboardRequest {
    pub widgets: Vec<PlacedWidget>,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The dashboard registry: every declared widget, shared like the other
/// primitives (cheap `Arc` clone) and reached through the request
/// extension by the framework handlers.
#[derive(Clone)]
pub struct Dashboards {
    inner: Arc<Inner>,
}

struct Inner {
    widgets: HashMap<&'static str, Arc<dyn WidgetDefinition>>,
}

impl std::fmt::Debug for Dashboards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dashboards")
            .field("widgets", &self.inner.widgets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Dashboards {
    /// Build the registry from declared widgets, rejecting duplicate
    /// names at boot. Called by the kernel after modules configure.
    pub fn build(defs: Vec<Arc<dyn WidgetDefinition>>) -> Result<Self> {
        let mut widgets = HashMap::new();
        for def in defs {
            let name = def.name();
            let span = def.default_span();
            if span == 0 || span > GRID_COLUMNS {
                return Err(Error::internal(format!(
                    "widget {name:?} declares a default span of {span}; spans are 1..={GRID_COLUMNS}"
                )));
            }
            if widgets.insert(name, def).is_some() {
                return Err(Error::internal(format!(
                    "two widgets are declared with the name {name:?}"
                )));
            }
        }
        Ok(Self { inner: Arc::new(Inner { widgets }) })
    }

    pub fn len(&self) -> usize {
        self.inner.widgets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.widgets.is_empty()
    }

    /// The declared dashboard names — a dashboard exists because widgets
    /// were declared for it.
    pub fn dashboards(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> =
            self.inner.widgets.values().map(|w| w.dashboard()).collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    pub fn contains_dashboard(&self, dashboard: &str) -> bool {
        self.inner.widgets.values().any(|w| w.dashboard() == dashboard)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn WidgetDefinition>> {
        self.inner.widgets.get(name)
    }

    /// Every widget declared for a dashboard, sorted by title for a
    /// stable catalogue.
    pub fn widgets_of(&self, dashboard: &str) -> Vec<&Arc<dyn WidgetDefinition>> {
        let mut defs: Vec<_> = self
            .inner
            .widgets
            .values()
            .filter(|w| w.dashboard() == dashboard)
            .collect();
        defs.sort_by_key(|w| w.title());
        defs
    }

    /// A dashboard's default layout: the widgets that declared a default
    /// position, in position order. What a user sees before customizing,
    /// and what "reset" returns them to.
    pub fn default_layout(&self, dashboard: &str) -> Vec<PlacedWidget> {
        let mut defs: Vec<_> = self
            .inner
            .widgets
            .values()
            .filter(|w| w.dashboard() == dashboard)
            .filter_map(|w| w.default_position().map(|p| (p, w)))
            .collect();
        defs.sort_by_key(|(p, w)| (*p, w.name()));
        defs.into_iter()
            .take(MAX_WIDGETS)
            .map(|(_, w)| PlacedWidget { widget: w.name().to_string(), span: w.default_span() as i32 })
            .collect()
    }

    /// The user's saved arrangement for a dashboard, if they made one.
    pub async fn layout(
        &self,
        db: &DatabaseConnection,
        user_id: Uuid,
        dashboard: &str,
    ) -> Result<Option<Vec<PlacedWidget>>> {
        layout_store::load(db, user_id, dashboard).await
    }

    /// Everything wrong with a proposed layout, checked before any write:
    /// the [`MAX_WIDGETS`] cap, unknown widgets, widgets of another
    /// dashboard, out-of-grid spans and duplicates. Permission checks are
    /// the caller's — the registry does not know who is asking.
    pub fn validate_layout(&self, dashboard: &str, widgets: &[PlacedWidget]) -> Result<()> {
        if widgets.len() > MAX_WIDGETS {
            return Err(Error::Validation(format!(
                "a dashboard holds at most {MAX_WIDGETS} widgets; this layout has {}",
                widgets.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for placed in widgets {
            let def = self.get(&placed.widget).ok_or_else(|| {
                Error::Validation(format!("there is no widget named {:?}", placed.widget))
            })?;
            if def.dashboard() != dashboard {
                return Err(Error::Validation(format!(
                    "the widget {:?} belongs to the {:?} dashboard",
                    placed.widget,
                    def.dashboard()
                )));
            }
            if !(1..=GRID_COLUMNS as i32).contains(&placed.span) {
                return Err(Error::Validation(format!(
                    "widget spans are 1..={GRID_COLUMNS} grid columns"
                )));
            }
            if !seen.insert(placed.widget.as_str()) {
                return Err(Error::Validation(format!(
                    "the widget {:?} is placed twice",
                    placed.widget
                )));
            }
        }
        Ok(())
    }

    /// Persist a user's arrangement, [`Dashboards::validate_layout`]-ing
    /// it first so no invalid layout is ever stored.
    pub async fn save_layout(
        &self,
        db: &DatabaseConnection,
        user_id: Uuid,
        dashboard: &str,
        widgets: &[PlacedWidget],
    ) -> Result<()> {
        self.validate_layout(dashboard, widgets)?;
        layout_store::save(db, user_id, dashboard, widgets).await
    }

    /// Forget a user's arrangement — back to the default layout.
    pub async fn reset_layout(
        &self,
        db: &DatabaseConnection,
        user_id: Uuid,
        dashboard: &str,
    ) -> Result<()> {
        layout_store::reset(db, user_id, dashboard).await
    }
}

// ---------------------------------------------------------------------------
// Layout persistence
// ---------------------------------------------------------------------------

/// Persistence for saved layouts — one row per (user, dashboard) in the
/// `dashboard_layouts` table, per database, so each tenant's users keep
/// their own arrangements next to the data the widgets show.
pub(crate) mod layout_store {
    use super::PlacedWidget;
    use crate::error::{Error, Result};
    use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
    use uuid::Uuid;

    pub(crate) async fn load(
        db: &DatabaseConnection,
        user_id: Uuid,
        dashboard: &str,
    ) -> Result<Option<Vec<PlacedWidget>>> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT widgets FROM dashboard_layouts WHERE user_id = $1 AND dashboard = $2",
            [user_id.into(), dashboard.into()],
        );
        let Some(row) = db.query_one(stmt).await? else {
            return Ok(None);
        };
        let value: serde_json::Value = row.try_get("", "widgets")?;
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| Error::internal(format!("stored dashboard layout did not parse: {e}")))
    }

    pub(crate) async fn save(
        db: &DatabaseConnection,
        user_id: Uuid,
        dashboard: &str,
        widgets: &[PlacedWidget],
    ) -> Result<()> {
        let value = serde_json::to_value(widgets).map_err(|e| Error::internal(e.to_string()))?;
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO dashboard_layouts (user_id, dashboard, widgets, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (user_id, dashboard) \
             DO UPDATE SET widgets = $3, updated_at = now()",
            [user_id.into(), dashboard.into(), value.into()],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    pub(crate) async fn reset(
        db: &DatabaseConnection,
        user_id: Uuid,
        dashboard: &str,
    ) -> Result<()> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "DELETE FROM dashboard_layouts WHERE user_id = $1 AND dashboard = $2",
            [user_id.into(), dashboard.into()],
        );
        db.execute(stmt).await?;
        Ok(())
    }
}
