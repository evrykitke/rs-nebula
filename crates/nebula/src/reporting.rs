//! The reporting engine: a framework primitive for building the documents
//! an ERP prints — invoices, registers, statements, dashboards as PDF.
//!
//! Reporting is foundational, so it lives in the framework and every app
//! reaches it through `ctx.reporting()` (and handlers through the
//! [`Reporting`] request extension), the same way storage, events, caching
//! and numbering are wired.
//!
//! The design is four layers, each decoupled by a trait so no single
//! choice is permanent:
//!
//! 1. **Widgets → document model → theme.** A report is composed of
//!    [`Widget`]s that carry a structured, serializable model; a
//!    [`ReportFormat`] theme decides how that model becomes output. The
//!    same report renders Modern, Compact or Corporate without being
//!    rewritten — the difference lives in the theme.
//! 2. **[`ReportRenderer`]** sits behind the model, so the output backend
//!    (Typst for PDF, a spreadsheet writer for Excel) is pluggable.
//! 3. **[`ReportDataSource`]** is the data port. A report declares the
//!    datasources it needs; the engine resolves them (async, from the
//!    database or other ports) before the report builds its widgets. Apps
//!    stay independent: they consume data through this port, not by
//!    depending on the module that owns it.
//! 4. **The report registry.** Apps declare reports in `configure`
//!    (`ctx.declare_report(...)`) exactly like numbering series; the kernel
//!    builds one registry and serves every report from `/reports/{name}`.

use crate::auth::authz::Authz;
use crate::auth::permission;
use crate::error::{Error, Result};
use crate::storage::Storage;
use crate::tenancy::{TenantManager, TenantRef};
use async_trait::async_trait;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Document model
// ---------------------------------------------------------------------------

/// One report: a title, optional subtitle and an ordered list of widgets.
/// Company chrome (logo, running header/footer) is added by the renderer
/// from the resolved [`CompanyInformation`], so a report body never has to
/// carry it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub orientation: Orientation,
    pub widgets: Vec<Widget>,
}

impl Report {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            orientation: Orientation::Portrait,
            widgets: Vec::new(),
        }
    }

    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn orientation(mut self, orientation: Orientation) -> Self {
        self.orientation = orientation;
        self
    }

    /// Append a widget, returning `self` for fluent construction.
    pub fn with(mut self, widget: Widget) -> Self {
        self.widgets.push(widget);
        self
    }
}

/// Page orientation. List reports with many columns use landscape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

/// A report element. Widgets carry data; the theme renders them. `type` is
/// the JSON discriminator so a theme can dispatch on it. Layout widgets
/// ([`Widget::Columns`], [`Widget::Group`]) nest other widgets, so a report
/// is a tree — an invoice is a `Columns` of bill-to and doc-meta blocks
/// above a `Table`, a dashboard is `Metrics` above `Chart`s, and so on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Widget {
    /// A section heading; `level` 1..=3 like h1..h3.
    Heading { level: u8, text: String },
    /// A run of text in one of a few semantic styles.
    Text { text: String, #[serde(default)] style: TextStyle },
    /// A bullet or numbered list.
    List { #[serde(default)] ordered: bool, items: Vec<String> },
    /// A labelled set of fields laid out in `columns` columns, e.g. a
    /// bill-to block or a document-meta block.
    KeyValues { title: Option<String>, items: Vec<KeyValue>, #[serde(default = "one")] columns: u8 },
    /// A row of KPI tiles (label, value, optional caption) — dashboards
    /// and summary bands ("top performing products", period totals).
    Metrics { items: Vec<Metric> },
    /// A data table — what list reports build on, and the only widget that
    /// also exports to Excel.
    Table(Table),
    /// A chart. Rendered to an embedded SVG by the PDF backend (Phase 2);
    /// carried in the model now so reports can declare it.
    Chart(Chart),
    /// An embedded image (diagrams, signatures, stamps). Not the company
    /// logo — that is chrome the renderer adds.
    Image(Image),
    /// A boxed note — terms and conditions, disclaimers, highlights.
    Callout(Callout),
    /// A horizontal progress/utilization bar — goals, quota, capacity.
    Progress(Progress),
    /// A QR code (payment links, document verification URLs). Rendered to
    /// an embedded image by the backend (Phase 2).
    QrCode { data: String, #[serde(default, skip_serializing_if = "Option::is_none")] caption: Option<String> },
    /// A 1-D barcode (document numbers, SKUs). Rendered by the backend
    /// (Phase 2).
    Barcode { data: String, #[serde(default)] symbology: Symbology, #[serde(default, skip_serializing_if = "Option::is_none")] caption: Option<String> },
    /// One or more signature lines laid out side by side — the sign-off
    /// band at the foot of orders, delivery notes, approvals.
    Signatures { items: Vec<Signature> },
    /// Place child widgets side by side. `widths` (optional) are relative
    /// weights per column; omitted means equal widths.
    Columns { columns: Vec<Vec<Widget>>, #[serde(default, skip_serializing_if = "Vec::is_empty")] widths: Vec<u16> },
    /// A titled, optionally boxed section grouping child widgets.
    Group(Group),
    /// A horizontal rule.
    Divider,
    /// Vertical whitespace of a given size.
    Spacer { #[serde(default)] size: SpaceSize },
    /// Force a page break before the next widget.
    PageBreak,
}

fn one() -> u8 {
    1
}

impl Widget {
    pub fn heading(level: u8, text: impl Into<String>) -> Self {
        Widget::Heading { level: level.clamp(1, 3), text: text.into() }
    }
    pub fn text(text: impl Into<String>) -> Self {
        Widget::Text { text: text.into(), style: TextStyle::Normal }
    }
    pub fn styled(text: impl Into<String>, style: TextStyle) -> Self {
        Widget::Text { text: text.into(), style }
    }
    pub fn bullets<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Widget::List { ordered: false, items: items.into_iter().map(Into::into).collect() }
    }
    pub fn numbered<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Widget::List { ordered: true, items: items.into_iter().map(Into::into).collect() }
    }
    pub fn key_values(items: Vec<KeyValue>) -> Self {
        Widget::KeyValues { title: None, items, columns: 1 }
    }
    pub fn metrics(items: Vec<Metric>) -> Self {
        Widget::Metrics { items }
    }
    pub fn columns(columns: Vec<Vec<Widget>>) -> Self {
        Widget::Columns { columns, widths: Vec::new() }
    }
    pub fn spacer(size: SpaceSize) -> Self {
        Widget::Spacer { size }
    }
}

/// Semantic text styles a theme maps to its own type scale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStyle {
    #[default]
    Normal,
    /// De-emphasized (captions, fine print).
    Muted,
    /// Emphasized body text.
    Strong,
    /// Small print (footnotes, legal).
    Small,
}

/// Whitespace sizes, resolved to real measures by the theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceSize {
    Small,
    #[default]
    Medium,
    Large,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyValue {
    pub label: String,
    pub value: String,
}

impl KeyValue {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { label: label.into(), value: value.into() }
    }
}

/// A KPI tile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// An optional trend hint the theme can colour (e.g. up = green).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<Trend>,
}

impl Metric {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { label: label.into(), value: value.into(), caption: None, trend: None }
    }
    pub fn caption(mut self, caption: impl Into<String>) -> Self {
        self.caption = Some(caption.into());
        self
    }
    pub fn trend(mut self, trend: Trend) -> Self {
        self.trend = Some(trend);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    Up,
    Down,
    Flat,
}

/// An embedded image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    /// File extension without the dot, e.g. `png`.
    pub format: String,
    /// Base64 image bytes.
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default)]
    pub align: Align,
}

/// A boxed note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Callout {
    #[serde(default)]
    pub style: CalloutStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub text: String,
}

impl Callout {
    pub fn new(style: CalloutStyle, text: impl Into<String>) -> Self {
        Self { style, title: None, text: text.into() }
    }
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
    pub fn into_widget(self) -> Widget {
        Widget::Callout(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalloutStyle {
    #[default]
    Info,
    Success,
    Warning,
    Muted,
}

/// A labelled progress bar. `value` is a fraction 0.0..=1.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub label: String,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

impl Progress {
    pub fn new(label: impl Into<String>, value: f64) -> Self {
        Self { label: label.into(), value: value.clamp(0.0, 1.0), caption: None }
    }
    pub fn caption(mut self, caption: impl Into<String>) -> Self {
        self.caption = Some(caption.into());
        self
    }
    pub fn into_widget(self) -> Widget {
        Widget::Progress(self)
    }
}

/// Barcode symbologies the backend can render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Symbology {
    #[default]
    Code128,
    Code39,
    Ean13,
    Ean8,
    UpcA,
}

/// A signature line: a rule to sign over, a role label, and optionally the
/// expected signatory's name and a date slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub dated: bool,
}

impl Signature {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), name: None, dated: false }
    }
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub fn dated(mut self) -> Self {
        self.dated = true;
        self
    }
}

/// A titled, optionally boxed grouping of widgets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Draw a card/border around the group.
    #[serde(default)]
    pub boxed: bool,
    pub widgets: Vec<Widget>,
}

impl Group {
    pub fn new(widgets: Vec<Widget>) -> Self {
        Self { title: None, boxed: false, widgets }
    }
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
    pub fn boxed(mut self) -> Self {
        self.boxed = true;
        self
    }
    pub fn into_widget(self) -> Widget {
        Widget::Group(self)
    }
}

/// A data table: labelled, aligned columns and string-rendered rows, with
/// an optional totals row the theme sets apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<Vec<String>>,
}

impl Table {
    pub fn new(columns: Vec<Column>) -> Self {
        Self { title: None, columns, rows: Vec::new(), totals: None }
    }
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
    pub fn row<I, S>(mut self, cells: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(cells.into_iter().map(Into::into).collect());
        self
    }
    pub fn totals<I, S>(mut self, cells: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.totals = Some(cells.into_iter().map(Into::into).collect());
        self
    }
    pub fn into_widget(self) -> Widget {
        Widget::Table(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub label: String,
    #[serde(default)]
    pub align: Align,
}

impl Column {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::Start }
    }
    /// Right-align — for amounts and other numbers.
    pub fn number(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::End }
    }
    pub fn center(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::Center }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// A chart definition. Kept minimal for now; the PDF backend renders it to
/// SVG in Phase 2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chart {
    pub kind: ChartKind,
    pub title: Option<String>,
    /// Category labels along the axis.
    pub labels: Vec<String>,
    /// One or more named series of values aligned to `labels`.
    pub series: Vec<Series>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Bar,
    /// Bars stacked per category (series summed).
    StackedBar,
    Line,
    /// Line with the area beneath filled.
    Area,
    Pie,
    /// Pie with a hollow centre.
    Donut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

// ---------------------------------------------------------------------------
// Formats and outputs
// ---------------------------------------------------------------------------

/// The visual theme applied at render time; UI-selectable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFormat {
    #[default]
    Modern,
    Compact,
    Corporate,
}

impl ReportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportFormat::Modern => "modern",
            ReportFormat::Compact => "compact",
            ReportFormat::Corporate => "corporate",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "modern" => Some(ReportFormat::Modern),
            "compact" => Some(ReportFormat::Compact),
            "corporate" => Some(ReportFormat::Corporate),
            _ => None,
        }
    }
}

/// The output file kind. Excel is only meaningful for table/list reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutput {
    #[default]
    Pdf,
    Excel,
}

impl ReportOutput {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pdf" => Some(ReportOutput::Pdf),
            "excel" | "xlsx" => Some(ReportOutput::Excel),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Data sources
// ---------------------------------------------------------------------------

/// What a datasource is handed to fetch its data: the request's
/// (tenant-swapped) database connection, the current tenant, and the
/// framework primitives a datasource might need.
pub struct DataCx<'a> {
    pub db: Option<&'a DatabaseConnection>,
    pub tenant: Option<&'a TenantRef>,
    pub tenants: Option<&'a Arc<TenantManager>>,
    pub storage: &'a Storage,
}

impl DataCx<'_> {
    /// The request database or a boot-facing error — for datasources that
    /// cannot function without one.
    pub fn require_db(&self) -> Result<&DatabaseConnection> {
        self.db
            .ok_or_else(|| Error::internal("this report requires a database connection"))
    }
}

/// A provider of report data. Object-safe (returns JSON) so a report can
/// hold a heterogeneous `Vec<Arc<dyn ReportDataSource>>`; the report reads
/// each back into a typed struct through [`ReportData::get`].
#[async_trait]
pub trait ReportDataSource: Send + Sync {
    /// Stable key the report uses to read this source's data back.
    fn key(&self) -> &'static str;
    /// Fetch the data as JSON.
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value>;
}

/// The resolved outputs of a report's datasources, keyed by
/// [`ReportDataSource::key`].
#[derive(Default)]
pub struct ReportData(HashMap<&'static str, serde_json::Value>);

impl ReportData {
    fn insert(&mut self, key: &'static str, value: serde_json::Value) {
        self.0.insert(key, value);
    }
    /// The raw JSON for a datasource, if it was declared.
    pub fn value(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }
    /// Read a datasource's data into a typed struct.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let value = self
            .0
            .get(key)
            .ok_or_else(|| Error::internal(format!("report data source {key:?} was not declared")))?;
        serde_json::from_value(value.clone())
            .map_err(|e| Error::internal(format!("report data source {key:?} did not match: {e}")))
    }
}

/// The tenant's company details for the report chrome (running header,
/// footer, cover). Produced by [`CompanyInformationDataSource`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompanyInformation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tax_pin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// The logo image bytes (PNG/JPEG), embedded when a logo is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<LogoImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoImage {
    /// File extension without the dot, e.g. `png`.
    pub format: String,
    /// Base64 image bytes (JSON-safe; decoded by the renderer).
    pub data_base64: String,
}

/// The framework datasource for the company running header — reads the
/// tenant's profile row and embeds its logo. Every report gets one
/// resolved automatically, so a report never fetches company data itself.
pub struct CompanyInformationDataSource;

#[async_trait]
impl ReportDataSource for CompanyInformationDataSource {
    fn key(&self) -> &'static str {
        "company"
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let mut info = CompanyInformation::default();
        if let (Some(tenants), Some(tenant)) = (cx.tenants, cx.tenant) {
            if let Some(row) = tenants.find_by_id(tenant.id).await? {
                info.name = row.display_name.clone();
                info.tax_pin = row.tax_pin.clone();
                info.vat_number = row.vat_number.clone();
                info.currency = row.default_currency.clone();
                info.logo = load_logo(cx.storage, row.logo_path.as_deref()).await;
            }
        }
        serde_json::to_value(info).map_err(|e| Error::internal(e.to_string()))
    }
}

/// Read logo bytes off the public store and base64-encode them for the
/// document model. A missing or unreadable file degrades to no logo.
async fn load_logo(storage: &Storage, logo_path: Option<&str>) -> Option<LogoImage> {
    let path = logo_path?;
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_string();
    let bytes = storage.read(path).await.ok()?;
    Some(LogoImage { format: ext, data_base64: base64_encode(&bytes) })
}

// ---------------------------------------------------------------------------
// Report definitions and the engine
// ---------------------------------------------------------------------------

/// A report an app declares: its identity, the datasources it needs, and
/// how to turn their resolved data into widgets.
pub trait ReportDefinition: Send + Sync {
    /// Unique name; also the URL segment (`/reports/{name}`).
    fn name(&self) -> &'static str;
    /// Human title shown in the UI and on the document.
    fn title(&self) -> &'static str;
    /// The default theme when the caller doesn't pick one.
    fn default_format(&self) -> ReportFormat {
        ReportFormat::Modern
    }
    /// Which outputs this report supports. List reports add Excel.
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    /// The permission a caller needs to render this report. `None` means
    /// any user of the tenant may view it. Reports over sensitive data
    /// should return their page permission here.
    fn permission(&self) -> Option<&'static str> {
        None
    }
    /// The datasources to resolve before building. The company datasource
    /// is always added by the engine, so reports list only their own.
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        Vec::new()
    }
    /// Build the report body from the resolved datasource data.
    fn build(&self, data: &ReportData) -> Result<Report>;
}

/// The fully-assembled document handed to a renderer: the resolved format
/// and watermark are baked in, so a renderer needs nothing else.
pub struct Document {
    pub company: CompanyInformation,
    pub report: Report,
    pub title: String,
    pub format: ReportFormat,
    /// Diagonal watermark text (e.g. "DRAFT", "COPY"), when the tenant set
    /// one. Drawn behind the content by the PDF backend.
    pub watermark: Option<String>,
}

/// Turns a [`Document`] into bytes.
pub trait ReportRenderer: Send + Sync {
    fn render(&self, doc: &Document) -> Result<Rendered>;
}

/// A tenant's report preferences, editable by an admin through the report
/// viewer. Persisted per tenant (per-database, like numbering overrides).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportSettings {
    /// The house format applied when a caller doesn't pick one. `None`
    /// falls back to each report's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_format: Option<ReportFormat>,
    /// A watermark drawn on every rendered report while set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<String>,
}

/// A rendered report: the bytes plus what to tell the browser.
pub struct Rendered {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub extension: &'static str,
}

/// What a render endpoint receives from the request to pass through to the
/// engine: the (tenant-swapped) connection and the current tenant.
pub struct RenderCx {
    pub db: Option<DatabaseConnection>,
    pub tenant: Option<TenantRef>,
}

/// The reporting engine: a registry of report definitions plus the
/// renderers, shared like the other primitives (cheap `Arc` clone).
#[derive(Clone)]
pub struct Reporting {
    inner: Arc<Inner>,
}

struct Inner {
    reports: HashMap<&'static str, Arc<dyn ReportDefinition>>,
    tenants: Option<Arc<TenantManager>>,
    storage: Storage,
    pdf: Arc<dyn ReportRenderer>,
    excel: Arc<dyn ReportRenderer>,
}

impl std::fmt::Debug for Reporting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporting")
            .field("reports", &self.inner.reports.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Reporting {
    /// Build the registry from declared reports (rejecting duplicate names)
    /// and wire the renderers. Called by the kernel after modules configure.
    pub(crate) fn build(
        defs: Vec<Arc<dyn ReportDefinition>>,
        tenants: Option<Arc<TenantManager>>,
        storage: Storage,
    ) -> Result<Self> {
        let mut reports = HashMap::new();
        for def in defs {
            let name = def.name();
            if reports.insert(name, def).is_some() {
                return Err(Error::internal(format!(
                    "two reports are declared with the name {name:?}"
                )));
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                reports,
                tenants,
                storage,
                pdf: renderers::pdf_renderer(),
                excel: renderers::excel_renderer(),
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.inner.reports.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.reports.is_empty()
    }
    /// The declared report names.
    pub fn names(&self) -> Vec<&'static str> {
        self.inner.reports.keys().copied().collect()
    }

    /// The permission a report requires to render, if declared.
    pub fn required_permission(&self, name: &str) -> Option<&'static str> {
        self.inner.reports.get(name).and_then(|def| def.permission())
    }

    /// Resolve a report's datasources, build it, and render to bytes. The
    /// effective format is the caller's choice, else the tenant's house
    /// default, else the report's own default.
    pub async fn render(
        &self,
        cx: &RenderCx,
        name: &str,
        format: Option<ReportFormat>,
        output: ReportOutput,
    ) -> Result<Rendered> {
        let def = self
            .inner
            .reports
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("report {name:?}")))?;

        if !def.outputs().contains(&output) {
            return Err(Error::Validation(format!(
                "report {name:?} does not support the requested output"
            )));
        }

        let settings = self.settings(cx.db.as_ref()).await;
        let format = format
            .or(settings.default_format)
            .unwrap_or_else(|| def.default_format());

        let datacx = DataCx {
            db: cx.db.as_ref(),
            tenant: cx.tenant.as_ref(),
            tenants: self.inner.tenants.as_ref(),
            storage: &self.inner.storage,
        };

        let company: CompanyInformation = {
            let value = CompanyInformationDataSource.load(&datacx).await?;
            serde_json::from_value(value).map_err(|e| Error::internal(e.to_string()))?
        };

        let mut data = ReportData::default();
        for source in def.data_sources() {
            data.insert(source.key(), source.load(&datacx).await?);
        }

        let report = def.build(&data)?;
        let doc = Document {
            company,
            title: def.title().to_string(),
            report,
            format,
            watermark: settings.watermark,
        };

        match output {
            ReportOutput::Pdf => self.inner.pdf.render(&doc),
            ReportOutput::Excel => self.inner.excel.render(&doc),
        }
    }

    /// The tenant's report settings, or defaults when unset or unreadable
    /// (settings must never block a render).
    pub async fn settings(&self, db: Option<&DatabaseConnection>) -> ReportSettings {
        let Some(db) = db else {
            return ReportSettings::default();
        };
        match settings_store::load(db).await {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(error = %e, "could not read report settings; using defaults");
                ReportSettings::default()
            }
        }
    }

    /// Persist the tenant's report settings (admin action from the viewer).
    pub async fn save_settings(
        &self,
        db: &DatabaseConnection,
        settings: &ReportSettings,
    ) -> Result<()> {
        settings_store::save(db, settings).await
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RenderParams {
    format: Option<String>,
    output: Option<String>,
}

/// The reporting routes, merged into the app by the kernel:
/// - `GET /reports/settings` — the tenant's report preferences
/// - `PUT /reports/settings` — set them (admin only)
/// - `GET /reports/{name}?format=modern&output=pdf` — render a report
pub(crate) fn routes() -> Router {
    Router::new()
        .route("/reports/settings", get(get_settings).put(put_settings))
        .route("/reports/{name}", get(render_report))
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

    let format = match params.format.as_deref() {
        Some(s) => Some(
            ReportFormat::parse(s)
                .ok_or_else(|| Error::Validation(format!("unknown report format {s:?}")))?,
        ),
        None => None,
    };
    let output = match params.output.as_deref() {
        Some(s) => ReportOutput::parse(s)
            .ok_or_else(|| Error::Validation(format!("unknown report output {s:?}")))?,
        None => ReportOutput::default(),
    };

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

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

mod renderers {
    use super::*;

    /// The PDF renderer. Typst-backed when the `reporting` feature is on;
    /// a debug JSON dump otherwise (and for now, until the theme lands).
    pub(super) fn pdf_renderer() -> Arc<dyn ReportRenderer> {
        Arc::new(DebugRenderer)
    }

    pub(super) fn excel_renderer() -> Arc<dyn ReportRenderer> {
        Arc::new(DebugRenderer)
    }

    /// A placeholder that serializes the assembled document to JSON, so the
    /// end-to-end pipeline (declare → resolve datasources → build widgets)
    /// can be verified before the real backends are wired.
    struct DebugRenderer;

    impl ReportRenderer for DebugRenderer {
        fn render(&self, doc: &Document) -> Result<Rendered> {
            let payload = serde_json::json!({
                "format": doc.format.as_str(),
                "watermark": doc.watermark,
                "title": doc.title,
                "company": doc.company,
                "report": doc.report,
            });
            let bytes = serde_json::to_vec_pretty(&payload)
                .map_err(|e| Error::internal(e.to_string()))?;
            Ok(Rendered { bytes, content_type: "application/json", extension: "json" })
        }
    }
}

/// Persistence for [`ReportSettings`] — a single row (`id = 1`) in the
/// `report_settings` table, per database (so each provisioned tenant keeps
/// its own house format and watermark).
mod settings_store {
    use super::*;
    use sea_orm::{ConnectionTrait, Statement};

    pub(super) async fn load(db: &DatabaseConnection) -> Result<ReportSettings> {
        let stmt = Statement::from_string(
            db.get_database_backend(),
            "SELECT default_format, watermark FROM report_settings WHERE id = 1",
        );
        match db.query_one(stmt).await? {
            Some(row) => {
                let default_format = row
                    .try_get::<Option<String>>("", "default_format")?
                    .and_then(|s| ReportFormat::parse(&s));
                let watermark = row.try_get::<Option<String>>("", "watermark")?;
                Ok(ReportSettings { default_format, watermark })
            }
            None => Ok(ReportSettings::default()),
        }
    }

    pub(super) async fn save(db: &DatabaseConnection, settings: &ReportSettings) -> Result<()> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO report_settings (id, default_format, watermark, updated_at) \
             VALUES (1, $1, $2, now()) \
             ON CONFLICT (id) \
             DO UPDATE SET default_format = $1, watermark = $2, updated_at = now()",
            [
                settings.default_format.map(|f| f.as_str().to_owned()).into(),
                watermark_value(settings.watermark.as_deref()).into(),
            ],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    /// Blank watermarks are stored as NULL, not an empty string.
    fn watermark_value(watermark: Option<&str>) -> Option<String> {
        watermark
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

/// Minimal base64 (standard alphabet, padded) so the model can carry image
/// bytes without pulling a dependency for one small use.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | (b[2] as u32);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn formats_and_outputs_parse() {
        assert_eq!(ReportFormat::parse("Compact"), Some(ReportFormat::Compact));
        assert_eq!(ReportFormat::parse("nope"), None);
        assert_eq!(ReportOutput::parse("xlsx"), Some(ReportOutput::Excel));
    }

    #[test]
    fn report_builds_fluently() {
        let report = Report::new("Test")
            .subtitle("sub")
            .with(Widget::heading(1, "Section"))
            .with(
                Table::new(vec![Column::new("A"), Column::number("B")])
                    .row(["1", "2"])
                    .totals(["", "2"])
                    .into_widget(),
            );
        assert_eq!(report.widgets.len(), 2);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["widgets"][0]["type"], "heading");
        assert_eq!(json["widgets"][1]["type"], "table");
    }
}
