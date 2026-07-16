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

use crate::error::{Error, Result};
use crate::jobs::Jobs;
use crate::storage::Storage;
use crate::tenancy::{TenantManager, TenantRef};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
    /// The document's own number, set on its own line under the title. A trade
    /// document is filed, quoted and chased by this number, so it is given its
    /// own line rather than trailing the title — carried without the `#`, which
    /// the renderer adds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    #[serde(default)]
    pub orientation: Orientation,
    /// What to call the downloaded file, without an extension. A report that
    /// renders a *particular* record should name itself after it: without
    /// this every invoice downloads as `sales-invoice.pdf`, and filing three
    /// of them means three files with the same name. `None` falls back to the
    /// report's registry name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub widgets: Vec<Widget>,
}

impl Report {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            number: None,
            orientation: Orientation::Portrait,
            file_name: None,
            widgets: Vec::new(),
        }
    }

    /// The document's own number, e.g. `SO-2026-00001`. Pass it bare: the
    /// renderer sets it under the title and adds the `#`.
    pub fn number(mut self, number: impl Into<String>) -> Self {
        self.number = Some(number.into());
        self
    }

    /// Name the downloaded file after the record this report renders, e.g. a
    /// document's own number.
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
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
    /// Absorb the table's spare width. Without a hint every text column
    /// shares the slack equally, which gives a line-number column the same
    /// width as a description — so mark the column that actually holds prose.
    /// When no column is marked, the text columns share it as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wide: bool,
}

impl Column {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::Start, wide: false }
    }
    /// Right-align — for amounts and other numbers.
    pub fn number(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::End, wide: false }
    }
    pub fn center(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::Center, wide: false }
    }
    /// The column that takes the leftover width — a description or a name.
    pub fn wide(label: impl Into<String>) -> Self {
        Self { label: label.into(), align: Align::Start, wide: true }
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
///
/// `Corporate` is the house look: formal stationery is what an ERP prints, so
/// it is what a report gets unless the caller, the tenant's settings, or the
/// report itself asks for something else.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFormat {
    Modern,
    Compact,
    #[default]
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

/// The output kind. `Excel` and `Table` are only meaningful for table/list
/// reports: `Excel` is a downloadable workbook; `Table` is an interactive,
/// in-app datatable (sortable/filterable/paginated by the client) served as
/// JSON — a "list report" the user works with on screen rather than a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutput {
    #[default]
    Pdf,
    Excel,
    Table,
}

impl ReportOutput {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportOutput::Pdf => "pdf",
            ReportOutput::Excel => "excel",
            ReportOutput::Table => "table",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pdf" => Some(ReportOutput::Pdf),
            "excel" | "xlsx" => Some(ReportOutput::Excel),
            "table" | "html" => Some(ReportOutput::Table),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Data sources
// ---------------------------------------------------------------------------

/// The arguments a report was asked for: everything in the render request's
/// query string that is not `format` or `output`.
///
/// This is what makes a report a *function* rather than a fixed view — a
/// document report is told which invoice to draw (`?id=…`), a register which
/// window to cover (`?from=…&to=…`). Values arrive as strings from the URL,
/// so the typed readers below are where a bad one is caught, once, with a
/// message naming the parameter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportParams(HashMap<String, String>);

impl ReportParams {
    pub fn new(values: HashMap<String, String>) -> Self {
        // A blank value is not an answer: `?from=` means "unset", not "".
        Self(values.into_iter().filter(|(_, v)| !v.trim().is_empty()).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw value, if the caller supplied one.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// A value the report cannot do without.
    pub fn require(&self, key: &str) -> Result<&str> {
        self.get(key)
            .ok_or_else(|| Error::Validation(format!("this report needs the {key:?} parameter")))
    }

    /// Parse a value, reporting *which* parameter was malformed rather than
    /// letting a parse error surface with no context.
    pub fn parse<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        match self.get(key) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|e| Error::Validation(format!("the {key:?} parameter is not valid: {e}"))),
        }
    }

    /// Parse a value the report cannot do without.
    pub fn require_parse<T>(&self, key: &str) -> Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        self.require(key)?;
        Ok(self.parse(key)?.expect("require checked the key is present"))
    }

    /// The record a document report draws.
    pub fn id(&self) -> Result<uuid::Uuid> {
        self.require_parse("id")
    }

    /// An optional date, e.g. a register's `from`/`to` bounds.
    pub fn date(&self, key: &str) -> Result<Option<chrono::NaiveDate>> {
        self.parse(key)
    }
}

/// What a datasource is handed to fetch its data: the request's
/// (tenant-swapped) database connection, the current tenant, the arguments
/// the report was asked for, and the framework primitives a datasource might
/// need.
pub struct DataCx<'a> {
    pub db: Option<&'a DatabaseConnection>,
    pub tenant: Option<&'a TenantRef>,
    pub tenants: Option<&'a Arc<TenantManager>>,
    pub storage: &'a Storage,
    /// The render request's arguments — how a datasource knows *which*
    /// record or window to load.
    pub params: &'a ReportParams,
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
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
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
                info.address = row.address.clone();
                info.email = row.email.clone();
                info.website = row.website.clone();
                info.phone = row.phone.clone();
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
    /// The group this report belongs to (usually the owning app/module,
    /// e.g. "Sales", "Accounting"). The viewer groups the catalogue into
    /// one collapsible accordion per group.
    fn group(&self) -> &'static str {
        "General"
    }
    /// The default theme when the caller doesn't pick one. Override only for
    /// a report whose shape genuinely needs another look.
    fn default_format(&self) -> ReportFormat {
        ReportFormat::default()
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
    /// What to call the file, without an extension — a document's own number
    /// where it has one. Filled in by the engine from
    /// [`Report::file_name`]; renderers do not set it.
    pub file_name: Option<String>,
}

/// What a render endpoint receives from the request to pass through to the
/// engine: the (tenant-swapped) connection, the current tenant, and the
/// arguments the report was asked for.
pub struct RenderCx {
    pub db: Option<DatabaseConnection>,
    pub tenant: Option<TenantRef>,
    pub params: ReportParams,
}

impl RenderCx {
    /// A render with no arguments — reports that take none, and tests.
    pub fn new(db: Option<DatabaseConnection>, tenant: Option<TenantRef>) -> Self {
        Self { db, tenant, params: ReportParams::default() }
    }

    pub fn with_params(mut self, params: ReportParams) -> Self {
        self.params = params;
        self
    }
}

/// A report's public metadata for the viewer catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportInfo {
    pub name: String,
    pub title: String,
    pub group: String,
    pub outputs: Vec<ReportOutput>,
    pub default_format: ReportFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_permission: Option<String>,
}

/// The interactive list-report payload: every table in a report, flattened
/// out of its widgets, for the viewer to render as sortable/filterable
/// datatables. This is the `table` output — an on-screen list report rather
/// than a downloadable file, but it lives in the reporting engine so a report
/// is authored once and offered as PDF, Excel, and/or an interactive table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportTables {
    pub title: String,
    pub tables: Vec<DataTable>,
}

/// One table in a [`ReportTables`] payload — the [`Table`] widget model
/// reshaped with the per-column hints a datatable UI needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub columns: Vec<DataColumn>,
    pub rows: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataColumn {
    pub label: String,
    pub align: Align,
    /// Numeric columns sort by value (not lexically) and align to the end.
    pub numeric: bool,
}

impl From<&Table> for DataTable {
    fn from(table: &Table) -> Self {
        DataTable {
            title: table.title.clone(),
            columns: table
                .columns
                .iter()
                .map(|c| DataColumn {
                    label: c.label.clone(),
                    align: c.align,
                    numeric: c.align == Align::End,
                })
                .collect(),
            rows: table.rows.clone(),
            totals: table.totals.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Background report generation
// ---------------------------------------------------------------------------

/// Redis queue that carries [`RenderReportJob`]s to the render worker.
pub(crate) const REPORT_QUEUE: &str = "report-render";

/// Lifecycle of a queued report render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportJobStatus {
    /// Accepted and waiting for a worker.
    Queued,
    /// A worker is building the document.
    Running,
    /// Rendered and the artifact is stored, ready to download.
    Completed,
    /// The build or render failed; see [`ReportJob::error`].
    Failed,
}

impl ReportJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportJobStatus::Queued => "queued",
            ReportJobStatus::Running => "running",
            ReportJobStatus::Completed => "completed",
            ReportJobStatus::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "running" => ReportJobStatus::Running,
            "completed" => ReportJobStatus::Completed,
            "failed" => ReportJobStatus::Failed,
            _ => ReportJobStatus::Queued,
        }
    }
}

/// A queued (or finished) background report render, as reported to the
/// viewer. Data-heavy reports are enqueued instead of rendered on the
/// request thread; the client polls [`ReportJob`] until it is `completed`,
/// then downloads the stored artifact. The artifact's storage path is not
/// exposed here — downloads go through an authenticated endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportJob {
    pub id: uuid::Uuid,
    /// The report's name (registry key).
    pub report: String,
    /// The report's display title, resolved from the registry.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ReportFormat>,
    pub output: ReportOutput,
    pub status: ReportJobStatus,
    /// Suggested download file name once completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<i64>,
    /// The failure message when `status` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Who queued it (user name), for the job history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// The apalis job payload: everything the worker needs to render off the
/// request thread. The tenant is carried by id so the worker re-resolves
/// its own connection (the request's connection does not outlive it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RenderReportJob {
    pub job_id: uuid::Uuid,
    pub tenant_id: Option<uuid::Uuid>,
    pub report: String,
    pub format: Option<ReportFormat>,
    pub output: ReportOutput,
    /// The arguments the report was queued with. Carried on the job (and
    /// persisted with the row) because a parameterized report renders a
    /// *different document* without them — a worker that dropped them would
    /// quietly produce the wrong one.
    #[serde(default)]
    pub params: ReportParams,
}

/// Worker state for [`run_report_job`]: the engine (which carries the
/// renderers, storage and tenant manager) plus the main database, used
/// when a job is not tenant-scoped (single-tenant deployments).
#[derive(Clone)]
pub(crate) struct ReportJobContext {
    pub reporting: Reporting,
    pub database: Option<DatabaseConnection>,
}

/// The apalis worker function registered by the kernel for [`REPORT_QUEUE`].
pub(crate) async fn run_report_job(
    job: RenderReportJob,
    ctx: apalis::prelude::Data<ReportJobContext>,
) -> Result<()> {
    ctx.reporting.run_job(ctx.database.as_ref(), job).await
}

/// The cron tick when the artifact pruner runs as an apalis worker.
#[derive(Debug, Default, Clone)]
pub(crate) struct PruneJobsTick;

/// Worker state for [`prune_jobs_tick`].
#[derive(Clone)]
pub(crate) struct PruneJobsContext {
    pub reporting: Reporting,
    pub database: Option<DatabaseConnection>,
    pub retention_days: u32,
}

pub(crate) async fn prune_jobs_tick(
    _tick: PruneJobsTick,
    ctx: apalis::prelude::Data<PruneJobsContext>,
) -> Result<()> {
    let deleted = ctx
        .reporting
        .prune_jobs(ctx.database.as_ref(), ctx.retention_days)
        .await?;
    if deleted > 0 {
        tracing::info!(deleted, "pruned expired report jobs and artifacts");
    }
    Ok(())
}

/// The plain-interval fallback when the job system is off: same sweep,
/// spawned by `App::serve` alongside the audit pruner. Failures are
/// logged and the loop keeps going.
pub(crate) fn spawn_job_pruner(
    reporting: Reporting,
    database: Option<DatabaseConnection>,
    retention_days: u32,
    interval_secs: u64,
) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(60)));
        loop {
            interval.tick().await;
            match reporting.prune_jobs(database.as_ref(), retention_days).await {
                Ok(0) => {}
                Ok(deleted) => {
                    tracing::info!(deleted, "pruned expired report jobs and artifacts")
                }
                Err(e) => tracing::error!(error = %e, "report artifact pruning pass failed"),
            }
        }
    });
}

/// Flatten a built document's tables into the interactive datatable payload.
fn tables_of(doc: &Document) -> ReportTables {
    let mut tables = Vec::new();
    collect_tables(&doc.report.widgets, &mut tables);
    ReportTables {
        title: doc.report.title.clone(),
        tables: tables.into_iter().map(DataTable::from).collect(),
    }
}

/// Depth-first collection of every table in a widget tree, descending into
/// layout widgets (groups, columns) so nested tables still surface.
fn collect_tables<'a>(widgets: &'a [Widget], out: &mut Vec<&'a Table>) {
    for w in widgets {
        match w {
            Widget::Table(t) => out.push(t),
            Widget::Group(g) => collect_tables(&g.widgets, out),
            Widget::Columns { columns, .. } => {
                for col in columns {
                    collect_tables(col, out);
                }
            }
            _ => {}
        }
    }
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

    /// The public catalogue of declared reports, sorted by name.
    pub fn catalogue(&self) -> Vec<ReportInfo> {
        let mut list: Vec<ReportInfo> = self
            .inner
            .reports
            .values()
            .map(|d| ReportInfo {
                name: d.name().to_string(),
                title: d.title().to_string(),
                group: d.group().to_string(),
                outputs: d.outputs().to_vec(),
                default_format: d.default_format(),
                requires_permission: d.permission().map(str::to_string),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
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
            .ok_or_else(|| Error::NotFound(format!("report {name:?}")))?
            .clone();

        if !def.outputs().contains(&output) {
            return Err(Error::Validation(format!(
                "report {name:?} does not support the requested output"
            )));
        }

        let doc = self.document(cx, def.as_ref(), format).await?;
        self.render_document(&doc, output)
    }

    /// Render an assembled document and stamp the file name the report chose,
    /// so a caller downloading a document gets a file named after the record
    /// rather than after the report.
    fn render_document(&self, doc: &Document, output: ReportOutput) -> Result<Rendered> {
        let file_name = doc.report.file_name.clone();
        let mut rendered = match output {
            ReportOutput::Pdf => self.inner.pdf.render(doc)?,
            ReportOutput::Excel => self.inner.excel.render(doc)?,
            ReportOutput::Table => {
                let bytes = serde_json::to_vec(&tables_of(doc))
                    .map_err(|e| Error::internal(e.to_string()))?;
                Rendered {
                    bytes,
                    content_type: "application/json",
                    extension: "json",
                    file_name: None,
                }
            }
        };
        rendered.file_name = file_name;
        Ok(rendered)
    }

    /// Render a caller-supplied [`Report`] that no [`ReportDefinition`] backs.
    ///
    /// This is the seam for **list exports**: an on-screen datatable already
    /// knows its columns, its formatting and the rows the user filtered to, so
    /// it hands that model over rather than a report re-deriving it (and
    /// drifting from what the screen showed). Everything downstream is shared
    /// with declared reports — the company letterhead, the tenant's house
    /// format and watermark, and the renderers — so an exported list is the
    /// same stationery as the rest of the catalogue.
    ///
    /// The rows come from the caller, so this renders *the caller's own data
    /// back to them*: it grants no read access that the list endpoints did not
    /// already give, and the result is a document, never a source of truth.
    pub async fn render_ad_hoc(
        &self,
        cx: &RenderCx,
        report: Report,
        format: Option<ReportFormat>,
        output: ReportOutput,
    ) -> Result<Rendered> {
        let doc = self.ad_hoc_document(cx, report, format).await?;
        self.render_document(&doc, output)
    }

    /// Wrap a caller-supplied report in the same chrome [`document`] gives a
    /// declared one: resolved company information, house format, watermark.
    async fn ad_hoc_document(
        &self,
        cx: &RenderCx,
        report: Report,
        format: Option<ReportFormat>,
    ) -> Result<Document> {
        let settings = self.settings(cx.db.as_ref()).await;
        let format = format
            .or(settings.default_format)
            .unwrap_or_default();

        let datacx = DataCx {
            db: cx.db.as_ref(),
            tenant: cx.tenant.as_ref(),
            tenants: self.inner.tenants.as_ref(),
            storage: &self.inner.storage,
            params: &cx.params,
        };
        let company: CompanyInformation = {
            let value = CompanyInformationDataSource.load(&datacx).await?;
            serde_json::from_value(value).map_err(|e| Error::internal(e.to_string()))?
        };

        Ok(Document {
            company,
            title: report.title.clone(),
            report,
            format,
            watermark: settings.watermark,
        })
    }

    /// Build a report and return its tables as an interactive datatable
    /// payload (the `table` output), for the viewer's list-report view. Uses
    /// the same document pipeline as PDF/Excel, so one report definition
    /// serves every output.
    pub async fn datatables(
        &self,
        cx: &RenderCx,
        name: &str,
        format: Option<ReportFormat>,
    ) -> Result<ReportTables> {
        let def = self
            .inner
            .reports
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("report {name:?}")))?
            .clone();

        if !def.outputs().contains(&ReportOutput::Table) {
            return Err(Error::Validation(format!(
                "report {name:?} does not support the table output"
            )));
        }

        let doc = self.document(cx, def.as_ref(), format).await?;
        Ok(tables_of(&doc))
    }

    /// Render a report to one themed SVG per page, for the in-app viewer. The
    /// preview always uses the PDF layout (there is no Excel preview), so the
    /// pages match a PDF download exactly. Requires the `reporting` feature.
    pub async fn preview(
        &self,
        cx: &RenderCx,
        name: &str,
        format: Option<ReportFormat>,
    ) -> Result<Vec<String>> {
        let def = self
            .inner
            .reports
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("report {name:?}")))?
            .clone();

        let doc = self.document(cx, def.as_ref(), format).await?;

        #[cfg(feature = "reporting")]
        {
            typst_backend::render_svg(&doc)
        }
        #[cfg(not(feature = "reporting"))]
        {
            let _ = doc;
            Err(Error::internal("report preview requires the reporting feature"))
        }
    }

    /// Resolve a report's format and datasources and assemble the [`Document`]
    /// that the renderers consume. Shared by [`render`](Self::render) and
    /// [`preview`](Self::preview).
    async fn document(
        &self,
        cx: &RenderCx,
        def: &dyn ReportDefinition,
        format: Option<ReportFormat>,
    ) -> Result<Document> {
        let settings = self.settings(cx.db.as_ref()).await;
        let format = format
            .or(settings.default_format)
            .unwrap_or_else(|| def.default_format());

        let datacx = DataCx {
            db: cx.db.as_ref(),
            tenant: cx.tenant.as_ref(),
            tenants: self.inner.tenants.as_ref(),
            storage: &self.inner.storage,
            params: &cx.params,
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
        Ok(Document {
            company,
            title: def.title().to_string(),
            report,
            format,
            watermark: settings.watermark,
        })
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

    /// A report's display title from the registry, falling back to its name
    /// (a job may outlive a report being un-declared).
    fn title_of(&self, name: &str) -> String {
        self.inner
            .reports
            .get(name)
            .map(|d| d.title().to_string())
            .unwrap_or_else(|| name.to_string())
    }

    /// Queue a report to be rendered in the background: record a `queued`
    /// row and push the job. The caller polls [`job`](Self::job) until it is
    /// `completed`, then downloads the artifact. Validates the report and
    /// output up front, so a bad request fails synchronously rather than in
    /// the worker.
    pub async fn enqueue_job(
        &self,
        cx: &RenderCx,
        jobs: &Jobs,
        name: &str,
        format: Option<ReportFormat>,
        output: ReportOutput,
        requested_by: Option<(uuid::Uuid, String)>,
    ) -> Result<ReportJob> {
        let def = self
            .inner
            .reports
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("report {name:?}")))?
            .clone();
        if !def.outputs().contains(&output) {
            return Err(Error::Validation(format!(
                "report {name:?} does not support the requested output"
            )));
        }
        let db = cx.db.as_ref().ok_or_else(|| {
            Error::internal("queueing a report requires a database connection")
        })?;

        let id = uuid::Uuid::new_v4();
        let (by_id, by_name) = match requested_by {
            Some((id, name)) => (Some(id), Some(name)),
            None => (None, None),
        };
        job_store::insert(db, id, name, format, output, &cx.params, by_name.as_deref(), by_id)
            .await?;

        jobs.enqueue(
            REPORT_QUEUE,
            RenderReportJob {
                job_id: id,
                tenant_id: cx.tenant.as_ref().map(|t| t.id),
                report: name.to_string(),
                format,
                output,
                params: cx.params.clone(),
            },
        )
        .await?;

        self.job(Some(db), id).await
    }

    /// One background job's current state.
    pub async fn job(&self, db: Option<&DatabaseConnection>, id: uuid::Uuid) -> Result<ReportJob> {
        let db = db.ok_or_else(|| Error::internal("report jobs require a database connection"))?;
        let mut job = job_store::load(db, id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("report job {id}")))?;
        job.title = self.title_of(&job.report);
        Ok(job)
    }

    /// The most recent background jobs (newest first), for the job history.
    pub async fn jobs(&self, db: Option<&DatabaseConnection>, limit: u64) -> Result<Vec<ReportJob>> {
        let db = db.ok_or_else(|| Error::internal("report jobs require a database connection"))?;
        let mut jobs = job_store::list(db, limit).await?;
        for job in &mut jobs {
            job.title = self.title_of(&job.report);
        }
        Ok(jobs)
    }

    /// A completed job's stored artifact: the job (for its content type and
    /// file name) plus the bytes, read back from storage. Errors if the job
    /// is not yet completed.
    pub(crate) async fn artifact(
        &self,
        db: Option<&DatabaseConnection>,
        id: uuid::Uuid,
    ) -> Result<(ReportJob, Vec<u8>)> {
        let conn = db.ok_or_else(|| Error::internal("report jobs require a database connection"))?;
        let job = self.job(Some(conn), id).await?;
        if job.status != ReportJobStatus::Completed {
            return Err(Error::Validation(format!(
                "report job {id} is {}",
                job.status.as_str()
            )));
        }
        let path = job_store::file_path(conn, id)
            .await?
            .ok_or_else(|| Error::internal("a completed report job has no stored artifact"))?;
        let bytes = self.inner.storage.read_private(&path).await?;
        Ok((job, bytes))
    }

    /// One pruning pass over one database: delete report jobs created
    /// before `cutoff`, removing each stored artifact first. Old rows
    /// with no artifact (failed, or never completed) are dropped too.
    /// Answers the number of jobs deleted.
    async fn prune_jobs_on(
        &self,
        db: &DatabaseConnection,
        cutoff: DateTime<Utc>,
    ) -> Result<u64> {
        let mut deleted = 0;
        for (id, path) in job_store::expired(db, cutoff).await? {
            if let Some(path) = path.as_deref().filter(|p| !p.is_empty())
                && let Err(e) = self.inner.storage.remove_private(path).await
            {
                // Leave the row so the artifact is retried next pass
                // rather than orphaned on disk.
                tracing::warn!(job = %id, path = %path, error = %e,
                    "could not remove an expired report artifact; keeping the job row");
                continue;
            }
            job_store::delete(db, id).await?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Prune expired background report jobs and their stored artifacts
    /// everywhere: the main database plus every active tenant's own
    /// database (tenants sharing the main database ride on the main
    /// pass). Called on an interval by the kernel; `retention_days == 0`
    /// disables pruning. Answers the number of jobs deleted.
    pub async fn prune_jobs(
        &self,
        main_db: Option<&DatabaseConnection>,
        retention_days: u32,
    ) -> Result<u64> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
        let mut deleted = 0;
        if let Some(db) = main_db {
            deleted += self.prune_jobs_on(db, cutoff).await?;
        }
        if let Some(manager) = &self.inner.tenants {
            for tenant in manager.find_all().await? {
                let has_own_db = tenant
                    .connection_string
                    .as_deref()
                    .is_some_and(|s| !s.is_empty());
                if !tenant.is_active || !has_own_db {
                    continue;
                }
                let db = manager.connection_for(&tenant).await?;
                deleted += self.prune_jobs_on(&db, cutoff).await?;
            }
        }
        Ok(deleted)
    }

    /// Execute a queued render off the request thread (the worker body):
    /// resolve the target database and tenant, build and render the report,
    /// store the artifact, and settle the job row. A build/render failure is
    /// recorded as `failed` and swallowed (it will not succeed on retry, so
    /// the queue should not loop on it); only an unresolvable target bubbles
    /// up for apalis to retry.
    pub(crate) async fn run_job(
        &self,
        main_db: Option<&DatabaseConnection>,
        job: RenderReportJob,
    ) -> Result<()> {
        let (db, tenant) = match job.tenant_id {
            Some(tid) => {
                let tenants = self.inner.tenants.as_ref().ok_or_else(|| {
                    Error::internal("report job targets a tenant but multitenancy is disabled")
                })?;
                let model = tenants
                    .find_by_id(tid)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("tenant {tid}")))?;
                let db = tenants.connection_for(&model).await?;
                let tref = TenantRef { id: model.id, name: model.name.clone() };
                (db, Some(tref))
            }
            None => {
                let db = main_db
                    .cloned()
                    .ok_or_else(|| Error::internal("report job has no database to run against"))?;
                (db, None)
            }
        };

        if let Err(e) = job_store::mark_running(&db, job.job_id).await {
            tracing::warn!(error = %e, job = %job.job_id, "could not mark report job running");
        }

        let cx = RenderCx { db: Some(db.clone()), tenant: tenant.clone(), params: job.params.clone() };
        match self.render(&cx, &job.report, job.format, job.output).await {
            Ok(rendered) => {
                let resource = format!(
                    "{}.{}",
                    rendered.file_name.as_deref().unwrap_or(&job.report),
                    rendered.extension
                );
                // Private storage: an artifact may hold financial data, so
                // it must only leave through the permission-checked
                // download endpoint — never the unauthenticated `/public`.
                let container = match &tenant {
                    Some(t) => self.inner.storage.private_tenant(t),
                    None => self.inner.storage.private_container("reports")?,
                };
                let stored = container.store(&resource, &rendered.bytes).await?;
                job_store::mark_completed(
                    &db,
                    job.job_id,
                    &stored.path,
                    rendered.content_type,
                    rendered.extension,
                    &resource,
                    rendered.bytes.len() as i64,
                )
                .await?;
                tracing::info!(job = %job.job_id, report = %job.report, "report job completed");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::error!(job = %job.job_id, report = %job.report, error = %msg,
                    "report job failed");
                if let Err(e2) = job_store::mark_failed(&db, job.job_id, &msg).await {
                    tracing::warn!(error = %e2, job = %job.job_id,
                        "could not record report job failure");
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

// The axum routes and handlers live in their own submodule, kept apart from
// the engine and document model above.
mod web;
pub(crate) use web::routes;

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

mod renderers {
    use super::*;

    /// The PDF renderer. Typst-backed when the `reporting` feature is on;
    /// a debug JSON dump otherwise.
    pub(super) fn pdf_renderer() -> Arc<dyn ReportRenderer> {
        #[cfg(feature = "reporting")]
        {
            Arc::new(super::typst_backend::TypstRenderer::new())
        }
        #[cfg(not(feature = "reporting"))]
        {
            Arc::new(DebugRenderer)
        }
    }

    pub(super) fn excel_renderer() -> Arc<dyn ReportRenderer> {
        #[cfg(feature = "reporting")]
        {
            Arc::new(super::excel_backend::XlsxRenderer::new())
        }
        #[cfg(not(feature = "reporting"))]
        {
            Arc::new(DebugRenderer)
        }
    }

    /// A placeholder that serializes the assembled document to JSON, so the
    /// end-to-end pipeline (declare → resolve datasources → build widgets)
    /// can be verified before the real backends are wired. Used only when
    /// the `reporting` feature is off.
    #[allow(dead_code)]
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
            Ok(Rendered { bytes, content_type: "application/json", extension: "json", file_name: None })
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

/// Persistence for background [`ReportJob`]s — the `report_jobs` table, one
/// row per queued render, per database (like [`settings_store`]).
mod job_store {
    use super::*;
    use sea_orm::{ConnectionTrait, QueryResult, Statement};

    const COLUMNS: &str = "id, report, format, output, status, file_name, content_type, \
                           byte_size, error, requested_by, created_at, completed_at";

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn insert(
        db: &DatabaseConnection,
        id: uuid::Uuid,
        report: &str,
        format: Option<ReportFormat>,
        output: ReportOutput,
        params: &ReportParams,
        requested_by: Option<&str>,
        requested_by_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        // The arguments are stored with the row, not only on the queue
        // message: the job history has to be able to say which document a
        // render was for, and a retried job must render the same one.
        let params_json = if params.is_empty() {
            None
        } else {
            Some(serde_json::to_string(params).map_err(|e| Error::internal(e.to_string()))?)
        };
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO report_jobs \
             (id, report, format, output, status, params, requested_by, requested_by_id, created_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, now())",
            [
                id.into(),
                report.into(),
                format.map(|f| f.as_str().to_owned()).into(),
                output.as_str().to_owned().into(),
                params_json.into(),
                requested_by.map(str::to_owned).into(),
                requested_by_id.into(),
            ],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    pub(super) async fn mark_running(db: &DatabaseConnection, id: uuid::Uuid) -> Result<()> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE report_jobs SET status = 'running', started_at = now() WHERE id = $1",
            [id.into()],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn mark_completed(
        db: &DatabaseConnection,
        id: uuid::Uuid,
        file_path: &str,
        content_type: &str,
        extension: &str,
        file_name: &str,
        byte_size: i64,
    ) -> Result<()> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE report_jobs SET status = 'completed', file_path = $2, content_type = $3, \
             extension = $4, file_name = $5, byte_size = $6, error = NULL, completed_at = now() \
             WHERE id = $1",
            [
                id.into(),
                file_path.into(),
                content_type.into(),
                extension.into(),
                file_name.into(),
                byte_size.into(),
            ],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    pub(super) async fn mark_failed(
        db: &DatabaseConnection,
        id: uuid::Uuid,
        error: &str,
    ) -> Result<()> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE report_jobs SET status = 'failed', error = $2, completed_at = now() \
             WHERE id = $1",
            [id.into(), error.into()],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    pub(super) async fn load(
        db: &DatabaseConnection,
        id: uuid::Uuid,
    ) -> Result<Option<ReportJob>> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            &format!("SELECT {COLUMNS} FROM report_jobs WHERE id = $1"),
            [id.into()],
        );
        match db.query_one(stmt).await? {
            Some(row) => Ok(Some(row_to_job(&row)?)),
            None => Ok(None),
        }
    }

    pub(super) async fn list(db: &DatabaseConnection, limit: u64) -> Result<Vec<ReportJob>> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            &format!("SELECT {COLUMNS} FROM report_jobs ORDER BY created_at DESC LIMIT $1"),
            [(limit as i64).into()],
        );
        db.query_all(stmt)
            .await?
            .iter()
            .map(row_to_job)
            .collect()
    }

    /// Jobs created before `cutoff` — id and stored artifact path — for
    /// the retention pruner.
    pub(super) async fn expired(
        db: &DatabaseConnection,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<(uuid::Uuid, Option<String>)>> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT id, file_path FROM report_jobs WHERE created_at < $1",
            [cutoff.into()],
        );
        db.query_all(stmt)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    row.try_get::<uuid::Uuid>("", "id")?,
                    row.try_get::<Option<String>>("", "file_path")?,
                ))
            })
            .collect()
    }

    pub(super) async fn delete(db: &DatabaseConnection, id: uuid::Uuid) -> Result<()> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "DELETE FROM report_jobs WHERE id = $1",
            [id.into()],
        );
        db.execute(stmt).await?;
        Ok(())
    }

    /// The stored artifact path for a job, if it has one.
    pub(super) async fn file_path(
        db: &DatabaseConnection,
        id: uuid::Uuid,
    ) -> Result<Option<String>> {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT file_path FROM report_jobs WHERE id = $1",
            [id.into()],
        );
        match db.query_one(stmt).await? {
            Some(row) => Ok(row.try_get::<Option<String>>("", "file_path")?),
            None => Ok(None),
        }
    }

    /// Map a selected row to a [`ReportJob`]; `title` is filled by the engine
    /// from the registry.
    fn row_to_job(row: &QueryResult) -> Result<ReportJob> {
        let output = ReportOutput::parse(&row.try_get::<String>("", "output")?)
            .unwrap_or_default();
        Ok(ReportJob {
            id: row.try_get("", "id")?,
            report: row.try_get("", "report")?,
            title: String::new(),
            format: row
                .try_get::<Option<String>>("", "format")?
                .and_then(|s| ReportFormat::parse(&s)),
            output,
            status: ReportJobStatus::parse(&row.try_get::<String>("", "status")?),
            file_name: row.try_get("", "file_name")?,
            content_type: row.try_get("", "content_type")?,
            byte_size: row.try_get("", "byte_size")?,
            error: row.try_get("", "error")?,
            requested_by: row.try_get("", "requested_by")?,
            created_at: row.try_get("", "created_at")?,
            completed_at: row.try_get("", "completed_at")?,
        })
    }
}

/// The Typst PDF backend: walks the document into Typst markup (with the
/// theme baked in per [`ReportFormat`]) and compiles it to a PDF. Images
/// (the logo, image widgets) are handed to Typst as virtual static files.
#[cfg(feature = "reporting")]
mod typst_backend {
    use super::*;
    use typst_as_lib::TypstEngine;
    use typst_as_lib::typst_kit_options::TypstKitFontOptions;
    use typst_layout::PagedDocument;

    pub(super) struct TypstRenderer;

    impl TypstRenderer {
        pub(super) fn new() -> Self {
            Self
        }
    }

    impl ReportRenderer for TypstRenderer {
        fn render(&self, doc: &Document) -> Result<Rendered> {
            let mut assets: Vec<(String, Vec<u8>)> = Vec::new();
            let source = emit(doc, &mut assets);

            // The resolver takes `&str` file ids; borrow the owned names.
            let resolver: Vec<(&str, Vec<u8>)> = assets
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.clone()))
                .collect();

            let engine = TypstEngine::builder()
                .main_file(source)
                .search_fonts_with(
                    TypstKitFontOptions::default()
                        .include_system_fonts(false)
                        .include_embedded_fonts(true),
                )
                .with_static_file_resolver(resolver)
                .build();

            let compiled = engine.compile::<PagedDocument>();
            let document = compiled
                .output
                .map_err(|e| Error::internal(format!("report typesetting failed: {e}")))?;
            let pdf = typst_pdf::pdf(&document, &Default::default())
                .map_err(|e| Error::internal(format!("report PDF export failed: {e:?}")))?;
            Ok(Rendered { bytes: pdf, content_type: "application/pdf", extension: "pdf", file_name: None })
        }
    }

    /// Compile the same document Typst uses for the PDF, but export one SVG
    /// per page — the source of the themed in-app preview. Reusing the PDF
    /// layout keeps the preview and the download pixel-identical.
    pub(super) fn render_svg(doc: &Document) -> Result<Vec<String>> {
        let mut assets: Vec<(String, Vec<u8>)> = Vec::new();
        let source = emit(doc, &mut assets);

        let resolver: Vec<(&str, Vec<u8>)> = assets
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.clone()))
            .collect();

        let engine = TypstEngine::builder()
            .main_file(source)
            .search_fonts_with(
                TypstKitFontOptions::default()
                    .include_system_fonts(false)
                    .include_embedded_fonts(true),
            )
            .with_static_file_resolver(resolver)
            .build();

        let document = engine
            .compile::<PagedDocument>()
            .output
            .map_err(|e| Error::internal(format!("report typesetting failed: {e}")))?;

        let opts = typst_svg::SvgOptions::default();
        Ok(document
            .pages()
            .iter()
            .map(|page| typst_svg::svg(page, &opts))
            .collect())
    }

    /// Per-format look. Sizes are Typst length literals; colours are hex
    /// without the leading `#`.
    struct Theme {
        body: &'static str,
        h1: &'static str,
        h2: &'static str,
        h3: &'static str,
        small: &'static str,
        accent: &'static str,
        muted: &'static str,
        rule: &'static str,
        header_fill: &'static str,
        /// The letterhead's address and contact lines. Its own size, a step
        /// above `small`: this is the company's address on its own
        /// stationery, and someone has to be able to read it and write back.
        contact: &'static str,
        top_margin: &'static str,
        /// Gap between the letterhead and the body; a smaller value floats the
        /// header down from the paper's top edge, giving it breathing room.
        header_ascent: &'static str,
        leading: &'static str,
        zebra: bool,
        /// Rule every cell, not just the row below. A ruled grid is what makes
        /// a column of figures read as a column — a business document is
        /// scanned across and down, and hairline row rules leave the eye to
        /// guess which price belongs to which line.
        grid: bool,
        /// Fill behind the header row of a table.
        head_fill: &'static str,
        /// Padding inside a table cell. Documents are dense on purpose: the
        /// reader wants the whole order on one page.
        cell_inset: &'static str,
        /// Padding inside a boxed block — the party and reference panels a
        /// document opens with. Their own value, not the table's: a table is
        /// read as a column of figures and wants to stay tight, while the
        /// blocks above it are read as facts and want room around them.
        block_inset: &'static str,
        /// The document number under the title. Navy across every format: the
        /// number is the one string on the page a reader comes looking for, and
        /// it should read as an identifier rather than as more of the heading.
        number_fill: &'static str,
    }

    fn theme(format: ReportFormat) -> Theme {
        match format {
            // Roomy, colourful, larger type.
            ReportFormat::Modern => Theme {
                body: "10.5pt",
                h1: "20pt",
                h2: "14pt",
                h3: "11pt",
                small: "8.5pt",
                accent: "2563eb",
                muted: "6b7280",
                rule: "e5e7eb",
                header_fill: "f8fafc",
                contact: "9pt",
                top_margin: "4.2cm",
                header_ascent: "0.5cm",
                leading: "0.75em",
                zebra: true,
                // The airy one: zebra banding already leads the eye across a
                // row, so a full grid would only add noise.
                grid: false,
                head_fill: "f1f5f9",
                cell_inset: "5pt",
                block_inset: "6pt",
                number_fill: "1e3a8a",
            },
            // Dense, near-monochrome, thin rules — an RDLC-style list look.
            ReportFormat::Compact => Theme {
                body: "9pt",
                h1: "14pt",
                h2: "11pt",
                h3: "9.5pt",
                small: "8pt",
                accent: "111827",
                muted: "6b7280",
                rule: "9ca3af",
                header_fill: "ffffff",
                contact: "8.5pt",
                top_margin: "2.8cm",
                header_ascent: "0.7cm",
                leading: "0.55em",
                zebra: false,
                grid: true,
                head_fill: "f3f4f6",
                cell_inset: "3pt",
                block_inset: "6pt",
                number_fill: "1e3a8a",
            },
            // Classic, restrained, serif-heavy corporate stationery.
            // The house look: ruled, dense, and squarely aligned — a trade
            // document, not a page of prose.
            ReportFormat::Corporate => Theme {
                body: "11pt",
                h1: "17pt",
                h2: "13.5pt",
                h3: "11.5pt",
                // The letterhead's contacts and every table's labels are set
                // at this size, so it has to stay comfortably readable in
                // print — not merely legible.
                small: "9.5pt",
                accent: "1f2937",
                muted: "4b5563",
                rule: "6b7280",
                header_fill: "f3f4f6",
                contact: "10.5pt",
                // The letterhead's own height plus its ascent, and a little
                // over: too little and the logo is clipped by the paper edge,
                // too much and the document starts halfway down the page.
                // Sized against the letterhead — raising the type or its
                // padding grows it, and this has to follow.
                top_margin: "5.4cm",
                header_ascent: "0.5cm",
                leading: "0.6em",
                zebra: false,
                grid: true,
                head_fill: "eef2f7",
                cell_inset: "4pt",
                block_inset: "9pt",
                number_fill: "1e3a8a",
            },
        }
    }

    fn color(hex: &str) -> String {
        format!("rgb(\"#{hex}\")")
    }

    fn escape(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\r', "")
            .replace('\n', "\\n")
    }

    /// A Typst string value for **markup** position (`#"..."`), rendered
    /// literally with no markup interpretation.
    fn lit(text: &str) -> String {
        format!("#\"{}\"", escape(text))
    }

    /// A Typst string literal for **code** position (function arguments):
    /// `"..."` with no leading `#`.
    fn str_lit(text: &str) -> String {
        format!("\"{}\"", escape(text))
    }

    fn align_word(a: Align) -> &'static str {
        match a {
            Align::Start => "left",
            Align::Center => "center",
            Align::End => "right",
        }
    }

    /// The Typst source for a document, registering any embedded images as
    /// virtual assets along the way. Visible to the module's tests, which
    /// assert on the markup rather than on rendered glyphs.
    pub(super) fn emit(doc: &Document, assets: &mut Vec<(String, Vec<u8>)>) -> String {
        let t = theme(doc.format);
        let mut s = String::new();

        // Page setup: margins, running header/footer, optional watermark.
        s.push_str("#set page(paper: \"a4\"");
        if doc.report.orientation == Orientation::Landscape {
            s.push_str(", flipped: true");
        }
        s.push_str(", margin: (top: ");
        s.push_str(t.top_margin);
        s.push_str(", bottom: 2.4cm, left: 1.8cm, right: 1.8cm)");
        s.push_str(", header-ascent: ");
        s.push_str(t.header_ascent);
        s.push_str(", header: [");
        s.push_str(&header(doc, &t, assets));
        s.push_str("]");
        s.push_str(", footer: [");
        s.push_str(&footer(doc, &t));
        s.push_str("]");
        if let Some(mark) = doc.watermark.as_deref().filter(|m| !m.is_empty()) {
            s.push_str(", background: place(center + horizon, rotate(-45deg, text(size: 96pt, fill: rgb(\"#9ca3af35\"), weight: \"bold\")[");
            s.push_str(&lit(mark));
            s.push_str("]))");
        }
        s.push_str(")\n");

        s.push_str("#set text(font: (\"Libertinus Serif\",), size: ");
        s.push_str(t.body);
        s.push_str(", fill: ");
        s.push_str(&color("111827"));
        s.push_str(")\n");
        s.push_str("#set par(leading: ");
        s.push_str(t.leading);
        s.push_str(", justify: false)\n\n");

        // Title bar: the title on the left, what qualifies it on the right,
        // closed by a rule. One band across the page rather than a heading
        // with a caption adrift beneath it — and it gives the blocks below a
        // top edge to align to.
        s.push_str("#block(width: 100%, above: 0em, below: 0.5em, inset: (bottom: 3pt), ");
        s.push_str("stroke: (bottom: 1pt + ");
        s.push_str(&color(t.accent));
        s.push_str("))[#grid(columns: (1fr, auto), align: (left + bottom, right + bottom),\n[");
        s.push_str("#text(size: ");
        s.push_str(t.h1);
        s.push_str(", weight: \"bold\", fill: ");
        s.push_str(&color(t.accent));
        s.push_str(")[");
        s.push_str(&lit(&doc.report.title));
        s.push(']');
        // The number on its own line beneath the title: a step down in size so
        // it reads as the document's identifier rather than as more heading,
        // and hashed the way anyone quoting it would write it.
        if let Some(number) = &doc.report.number {
            s.push_str("\\\n#text(size: ");
            s.push_str(t.h2);
            s.push_str(", weight: \"bold\", fill: ");
            s.push_str(&color(t.number_fill));
            s.push_str(")[");
            s.push_str(&lit(&format!("#{number}")));
            s.push(']');
        }
        s.push_str("], [");
        if let Some(sub) = &doc.report.subtitle {
            s.push_str("#text(size: ");
            s.push_str(t.small);
            s.push_str(", fill: ");
            s.push_str(&color(t.muted));
            s.push_str(")[");
            s.push_str(&lit(sub));
            s.push(']');
        }
        s.push_str("])]\n");

        for widget in &doc.report.widgets {
            s.push_str(&widget_markup(widget, &t, assets, true));
            s.push('\n');
        }
        s
    }

    /// The company logo in a box of exactly [`LOGO_BOX`], if a logo is set;
    /// registers the image bytes as a virtual asset.
    ///
    /// The box is fixed and the image is `fit: "contain"`, so the letterhead
    /// occupies the same space whatever the tenant uploaded: a tall logo
    /// scales down instead of pushing the header off the top of the page, and
    /// a wide one is not stretched to fit. Scaling by height alone let either
    /// dimension run away — which is how a logo ends up clipped by the paper
    /// edge.
    fn logo_markup(
        company: &CompanyInformation,
        assets: &mut Vec<(String, Vec<u8>)>,
        height: &str,
    ) -> Option<String> {
        let logo = company.logo.as_ref()?;
        let bytes = base64_decode(&logo.data_base64)?;
        let name = format!("logo.{}", sanitize_ext(&logo.format));
        assets.push((name.clone(), bytes));
        Some(format!(
            "#box(width: {LOGO_MAX_WIDTH}, height: {height})[\
             #image({}, width: 100%, height: 100%, fit: \"contain\")]",
            str_lit(&name)
        ))
    }

    /// The widest a logo may print. Bounded so a long horizontal logo cannot
    /// crowd out the contact block beside it.
    const LOGO_MAX_WIDTH: &str = "3.4cm";

    /// The height a logo prints at in the letterhead. Fixed, so the header is
    /// the same height for every tenant and the page's top margin can be set
    /// to clear it — the letterhead must not resize itself around whatever
    /// image was uploaded.
    const LOGO_HEIGHT: &str = "1.1cm";

    /// The height a logo prints at when it sits inline beside the company
    /// name (the Compact letterhead's single band) rather than above it.
    const LOGO_HEIGHT_INLINE: &str = "0.75cm";

    /// The brand mark for the letterhead: the tenant's uploaded logo, or
    /// nothing. A tenant that has set no logo gets a letterhead of its name
    /// and contacts alone — an invented mark would be a graphic the company
    /// never chose, printed on its stationery. `dim` is the mark's height.
    fn brand_mark(
        company: &CompanyInformation,
        assets: &mut Vec<(String, Vec<u8>)>,
        dim: &str,
    ) -> String {
        logo_markup(company, assets, dim).unwrap_or_default()
    }

    /// The company's contact lines for the header — address (one entry per
    /// line), phone, email, website — each escaped for markup.
    fn contact_bits(c: &CompanyInformation) -> Vec<String> {
        let mut bits = Vec::new();
        if let Some(addr) = c.address.as_deref().filter(|s| !s.trim().is_empty()) {
            for line in addr.lines().map(str::trim).filter(|l| !l.is_empty()) {
                bits.push(lit(line));
            }
        }
        if let Some(p) = c.phone.as_deref().filter(|s| !s.trim().is_empty()) {
            bits.push(lit(&format!("Tel: {}", p.trim())));
        }
        if let Some(e) = c.email.as_deref().filter(|s| !s.trim().is_empty()) {
            bits.push(lit(e.trim()));
        }
        if let Some(w) = c.website.as_deref().filter(|s| !s.trim().is_empty()) {
            bits.push(lit(w.trim()));
        }
        bits
    }

    /// The tax-registration lines (PIN, VAT), each escaped for markup.
    fn tax_bits(c: &CompanyInformation) -> Vec<String> {
        let mut bits = Vec::new();
        if let Some(pin) = c.tax_pin.as_deref().filter(|s| !s.trim().is_empty()) {
            bits.push(lit(&format!("PIN: {}", pin.trim())));
        }
        if let Some(vat) = c.vat_number.as_deref().filter(|s| !s.trim().is_empty()) {
            bits.push(lit(&format!("VAT: {}", vat.trim())));
        }
        bits
    }

    /// The running header — a per-theme letterhead, so the three formats
    /// read as distinct stationery rather than the same block recoloured.
    fn header(doc: &Document, t: &Theme, assets: &mut Vec<(String, Vec<u8>)>) -> String {
        match doc.format {
            ReportFormat::Modern => header_modern(doc, t, assets),
            ReportFormat::Compact => header_compact(doc, t, assets),
            ReportFormat::Corporate => header_corporate(doc, t, assets),
        }
    }

    /// Modern: a soft-filled band, brand (logo over name in the accent
    /// colour) on the left, a right-aligned contact stack, a bold accent
    /// underline.
    fn header_modern(doc: &Document, t: &Theme, assets: &mut Vec<(String, Vec<u8>)>) -> String {
        let c = &doc.company;
        let mut left = String::new();
        let mark = brand_mark(c, assets, LOGO_HEIGHT);
        if !mark.is_empty() {
            left.push_str(&mark);
            if !c.name.is_empty() {
                left.push_str("\n#v(5pt)\n");
            }
        }
        if !c.name.is_empty() {
            left.push_str(&format!(
                "#text(size: {}, weight: \"bold\", fill: {})[{}]",
                t.h2,
                color(t.accent),
                lit(&c.name)
            ));
        }

        let mut bits = contact_bits(c);
        bits.extend(tax_bits(c));
        let right = if bits.is_empty() {
            String::new()
        } else {
            format!(
                "#align(right + horizon, text(size: {}, fill: {})[{}])",
                t.contact,
                color(t.muted),
                bits.join(" \\ ")
            )
        };

        format!(
            "#block(width: 100%, inset: 10pt, radius: 3pt, fill: {}, stroke: (bottom: 1.6pt + {}))\
             [#grid(columns: (1fr, auto), align: (left + horizon, right + horizon), gutter: 16pt, \
             [{}], [{}])]",
            color(t.header_fill),
            color(t.accent),
            left,
            right
        )
    }

    /// Compact: a tight single band — small inline logo and name on the
    /// left, contacts condensed to one middot-separated line on the right,
    /// a hairline rule. No fill.
    fn header_compact(doc: &Document, t: &Theme, assets: &mut Vec<(String, Vec<u8>)>) -> String {
        let c = &doc.company;
        let mut left = String::new();
        let mark = brand_mark(c, assets, LOGO_HEIGHT_INLINE);
        if !mark.is_empty() {
            left.push_str(&format!("#box(baseline: 30%)[{mark}] "));
        }
        if !c.name.is_empty() {
            left.push_str(&format!(
                "#text(size: {}, weight: \"bold\")[{}]",
                t.h3,
                lit(&c.name)
            ));
        }

        let mut bits = contact_bits(c);
        bits.extend(tax_bits(c));
        let right = if bits.is_empty() {
            String::new()
        } else {
            format!(
                "#align(right + horizon, text(size: {}, fill: {})[{}])",
                t.contact,
                color(t.muted),
                bits.join(" · ")
            )
        };

        format!(
            "#block(width: 100%, inset: (bottom: 4pt), stroke: (bottom: 0.5pt + {}))\
             [#grid(columns: (1fr, 1fr), align: (left + horizon, right + horizon), [{}], [{}])]",
            color(t.rule),
            left,
            right
        )
    }

    /// Corporate: a centred letterhead — logo, name in tracked caps, a
    /// centred contact line, closed by a double rule. Formal stationery.
    fn header_corporate(doc: &Document, t: &Theme, assets: &mut Vec<(String, Vec<u8>)>) -> String {
        let c = &doc.company;
        let mut inner = String::new();
        let mark = brand_mark(c, assets, LOGO_HEIGHT);
        if !mark.is_empty() {
            inner.push_str(&format!("#align(center)[{mark}]\n#v(5pt)\n"));
        }
        if !c.name.is_empty() {
            inner.push_str(&format!(
                "#align(center, text(size: {}, weight: \"bold\", tracking: 0.08em, fill: {})[{}])\n",
                t.h2,
                color(t.accent),
                lit(&c.name.to_uppercase())
            ));
        }
        // Two tidy centred lines rather than one long wrapping blob: the
        // address on one, the remaining contacts + tax ids on the next.
        let address_line: Vec<String> = c
            .address
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(lit)
            .collect();
        let mut detail_line = Vec::new();
        if let Some(p) = c.phone.as_deref().filter(|s| !s.trim().is_empty()) {
            detail_line.push(lit(&format!("Tel: {}", p.trim())));
        }
        if let Some(e) = c.email.as_deref().filter(|s| !s.trim().is_empty()) {
            detail_line.push(lit(e.trim()));
        }
        if let Some(w) = c.website.as_deref().filter(|s| !s.trim().is_empty()) {
            detail_line.push(lit(w.trim()));
        }
        detail_line.extend(tax_bits(c));
        for line in [address_line, detail_line] {
            if !line.is_empty() {
                inner.push_str(&format!(
                    "#v(4pt)\n#align(center, text(size: {}, fill: {})[{}])\n",
                    t.contact,
                    color(t.muted),
                    line.join("  ·  ")
                ));
            }
        }

        format!(
            "#block(width: 100%, inset: (bottom: 6pt))[{inner}#v(6pt)\
             #line(length: 100%, stroke: 0.8pt + {rule})]",
            rule = color(t.rule)
        )
    }

    /// The running footer — company name, optional website, and page X of Y.
    fn footer(doc: &Document, t: &Theme) -> String {
        let c = &doc.company;
        let left = if c.name.is_empty() {
            String::new()
        } else {
            lit(&c.name)
        };
        let center = c
            .website
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|w| lit(w.trim()))
            .unwrap_or_default();
        format!(
            "#block(width: 100%, inset: (top: 6pt), stroke: (top: 0.6pt + {rule}))\
             [#text(size: {small}, fill: {muted})[#grid(columns: (1fr, 1fr, 1fr), \
             align: (left + horizon, center + horizon, right + horizon), [{left}], [{center}], \
             [#context [Page #counter(page).display() of #counter(page).final().first()]])]]",
            rule = color(t.rule),
            small = t.small,
            muted = color(t.muted),
            left = left,
            center = center
        )
    }

    /// Emit a list of widgets into one content string (for columns/groups).
    fn children(
        widgets: &[Widget],
        t: &Theme,
        assets: &mut Vec<(String, Vec<u8>)>,
        full_width: bool,
    ) -> String {
        let mut s = String::new();
        for w in widgets {
            s.push_str(&widget_markup(w, t, assets, full_width));
            s.push('\n');
        }
        s
    }

    fn widget_markup(
        widget: &Widget,
        t: &Theme,
        assets: &mut Vec<(String, Vec<u8>)>,
        full_width: bool,
    ) -> String {
        match widget {
            Widget::Heading { level, text } => {
                let size = match level {
                    1 => t.h1,
                    2 => t.h2,
                    _ => t.h3,
                };
                format!(
                    "#block(above: 0.9em, below: 0.5em)[#text(size: {size}, weight: \"bold\", fill: {})[{}]]",
                    color(t.accent),
                    lit(text)
                )
            }
            Widget::Text { text, style } => {
                let (size, weight, fill) = match style {
                    TextStyle::Normal => (t.body, "regular", color("111827")),
                    TextStyle::Muted => (t.small, "regular", color(t.muted)),
                    TextStyle::Strong => (t.body, "bold", color("111827")),
                    TextStyle::Small => (t.small, "regular", color("111827")),
                };
                format!(
                    "#block(below: 0.5em)[#text(size: {size}, weight: \"{weight}\", fill: {fill})[{}]]",
                    lit(text)
                )
            }
            Widget::List { ordered, items } => {
                let func = if *ordered { "enum" } else { "list" };
                let mut s = format!("#{func}(");
                for item in items {
                    s.push('[');
                    s.push_str(&lit(item));
                    s.push_str("], ");
                }
                s.push(')');
                s
            }
            Widget::KeyValues { title, items, columns } => {
                let mut s = String::new();
                if let Some(title) = title {
                    // A block heading, not a section heading: this labels the
                    // fields beneath it ("Bill to"), it does not open a part
                    // of the document.
                    s.push_str("#block(below: 0.35em)[#text(size: ");
                    s.push_str(t.small);
                    s.push_str(", weight: \"bold\", fill: ");
                    s.push_str(&color(t.accent));
                    s.push_str(")[");
                    s.push_str(&lit(&title.to_uppercase()));
                    s.push_str("]]\n");
                }
                // A label-less pair is a run of lines, not a field: an address
                // block has nothing to put in the label column, and forcing it
                // through the grid indents the value against nothing.
                let unlabelled = items.iter().all(|kv| kv.label.trim().is_empty());
                if unlabelled {
                    s.push_str("#block(width: 100%)[");
                    let lines: Vec<String> = items
                        .iter()
                        .flat_map(|kv| kv.value.lines())
                        .map(|l| lit(l.trim()))
                        .collect();
                    s.push_str(&lines.join(" \\ "));
                    s.push(']');
                    return s;
                }
                let cols = (*columns).max(1) as usize;
                s.push_str("#grid(columns: (");
                for _ in 0..cols {
                    s.push_str("auto, 1fr, ");
                }
                s.push_str("), column-gutter: 8pt, row-gutter: 3pt,\n");
                for kv in items {
                    s.push_str("[#text(fill: ");
                    s.push_str(&color(t.muted));
                    s.push_str(", size: ");
                    s.push_str(t.small);
                    s.push_str(")[");
                    s.push_str(&lit(&kv.label));
                    s.push_str("]], [#text(size: ");
                    s.push_str(t.small);
                    s.push_str(")[");
                    s.push_str(&lit(&kv.value));
                    s.push_str("]], ");
                }
                s.push_str(")");
                s
            }
            Widget::Metrics { items } => {
                let mut s = String::from("#grid(columns: (");
                for _ in items {
                    s.push_str("1fr, ");
                }
                s.push_str("), gutter: 8pt,\n");
                for m in items {
                    s.push_str("[#block(width: 100%, inset: 8pt, radius: 4pt, stroke: 0.6pt + ");
                    s.push_str(&color(t.rule));
                    s.push_str(")[#text(size: ");
                    s.push_str(t.small);
                    s.push_str(", fill: ");
                    s.push_str(&color(t.muted));
                    s.push_str(")[");
                    s.push_str(&lit(&m.label));
                    s.push_str("] \\ #text(size: ");
                    s.push_str(t.h2);
                    s.push_str(", weight: \"bold\")[");
                    s.push_str(&lit(&m.value));
                    s.push_str("]");
                    if let Some(cap) = &m.caption {
                        let fill = match m.trend {
                            Some(Trend::Up) => "16a34a",
                            Some(Trend::Down) => "dc2626",
                            _ => t.muted,
                        };
                        s.push_str(" \\ #text(size: ");
                        s.push_str(t.small);
                        s.push_str(", fill: ");
                        s.push_str(&color(fill));
                        s.push_str(")[");
                        s.push_str(&lit(cap));
                        s.push_str("]");
                    }
                    s.push_str("]], ");
                }
                s.push(')');
                s
            }
            Widget::Table(table) => table_markup(table, t, full_width),
            Widget::Chart(chart) => chart_markup(chart, t, assets),
            Widget::Image(image) => {
                if let Some(bytes) = base64_decode(&image.data_base64) {
                    let name = format!("asset-{}.{}", assets.len(), sanitize_ext(&image.format));
                    assets.push((name.clone(), bytes));
                    let mut s = format!(
                        "#align({})[#image({}, width: 60%)",
                        align_word(image.align),
                        str_lit(&name)
                    );
                    if let Some(cap) = &image.caption {
                        s.push_str(" \\ #text(size: ");
                        s.push_str(t.small);
                        s.push_str(", fill: ");
                        s.push_str(&color(t.muted));
                        s.push_str(")[");
                        s.push_str(&lit(cap));
                        s.push_str("]");
                    }
                    s.push(']');
                    s
                } else {
                    String::new()
                }
            }
            Widget::Callout(callout) => {
                let (fill, border) = match callout.style {
                    CalloutStyle::Info => ("eff6ff", "3b82f6"),
                    CalloutStyle::Success => ("f0fdf4", "22c55e"),
                    CalloutStyle::Warning => ("fffbeb", "f59e0b"),
                    CalloutStyle::Muted => ("f9fafb", "9ca3af"),
                };
                let mut s = format!(
                    "#block(width: 100%, fill: {}, stroke: (left: 3pt + {}), inset: 8pt, radius: 2pt, above: 0.6em, below: 0.6em)[",
                    color(fill),
                    color(border)
                );
                if let Some(title) = &callout.title {
                    s.push_str("#strong[");
                    s.push_str(&lit(title));
                    s.push_str("] \\ ");
                }
                s.push_str(&lit(&callout.text));
                s.push(']');
                s
            }
            Widget::Progress(p) => {
                let pct = (p.value.clamp(0.0, 1.0) * 100.0).round() as i64;
                let mut s = String::from("#block(below: 0.6em)[");
                s.push_str("#text(size: ");
                s.push_str(t.small);
                s.push_str(")[");
                s.push_str(&lit(&p.label));
                s.push_str("] \\ #box(width: 100%, height: 9pt, radius: 4pt, fill: ");
                s.push_str(&color(t.rule));
                s.push_str(")[#box(width: ");
                s.push_str(&pct.to_string());
                s.push_str("%, height: 9pt, radius: 4pt, fill: ");
                s.push_str(&color(t.accent));
                s.push_str(")]");
                if let Some(cap) = &p.caption {
                    s.push_str(" #text(size: ");
                    s.push_str(t.small);
                    s.push_str(", fill: ");
                    s.push_str(&color(t.muted));
                    s.push_str(")[");
                    s.push_str(&lit(cap));
                    s.push_str("]");
                }
                s.push(']');
                s
            }
            Widget::QrCode { data, caption } => placeholder_box("QR", data, caption.as_deref(), t),
            Widget::Barcode { data, caption, .. } => {
                placeholder_box("BARCODE", data, caption.as_deref(), t)
            }
            Widget::Signatures { items } => {
                // Boxed panels of equal size: a signature needs somewhere
                // definite to go, and equal boxes say the sign-offs carry
                // equal weight. The date sits at the foot of each box so the
                // signatures share a baseline however long the labels run.
                let mut s = String::from("#grid(columns: (");
                for _ in items {
                    s.push_str("1fr, ");
                }
                s.push_str("), gutter: 8pt,\n");
                for sig in items {
                    s.push_str("[#block(width: 100%, height: 2.1cm, inset: 5pt, stroke: 0.5pt + ");
                    s.push_str(&color(t.rule));
                    s.push_str(")[#text(size: ");
                    s.push_str(t.small);
                    s.push_str(", weight: \"bold\")[");
                    s.push_str(&lit(&sig.label));
                    s.push(']');
                    if let Some(name) = &sig.name {
                        s.push_str(" \\ #text(size: ");
                        s.push_str(t.small);
                        s.push_str(")[");
                        s.push_str(&lit(name));
                        s.push(']');
                    }
                    if sig.dated {
                        s.push_str("#place(bottom + left)[#text(size: ");
                        s.push_str(t.small);
                        s.push_str(", fill: ");
                        s.push_str(&color(t.muted));
                        s.push_str(")[");
                        s.push_str(&lit("Date: ____________"));
                        s.push_str("]]");
                    }
                    s.push_str("]], ");
                }
                s.push(')');
                s
            }
            Widget::Columns { columns, widths } => {
                let mut s = String::from("#grid(columns: (");
                if widths.len() == columns.len() && !widths.is_empty() {
                    for w in widths {
                        s.push_str(&w.to_string());
                        s.push_str("fr, ");
                    }
                } else {
                    for _ in columns {
                        s.push_str("1fr, ");
                    }
                }
                s.push_str("), column-gutter: 14pt,\n");
                for col in columns {
                    s.push('[');
                    // Content fills its column, so the blocks in a row share
                    // their edges: a totals table in the right-hand column
                    // ends where the lines table above it ends, instead of
                    // floating in the middle of the page.
                    s.push_str(&children(col, t, assets, true));
                    s.push_str("], ");
                }
                s.push(')');
                s
            }
            Widget::Group(group) => {
                let inner = children(&group.widgets, t, assets, full_width);
                let mut s = String::new();
                let body = if let Some(title) = &group.title {
                    let mut b = String::new();
                    // A boxed block is labelled, not headed: a small caps
                    // label reads as the box's name, where a section heading
                    // would claim the box opens a new part of the document.
                    if group.boxed {
                        b.push_str("#block(below: 0.35em)[#text(size: ");
                        b.push_str(t.small);
                        b.push_str(", weight: \"bold\", fill: ");
                        b.push_str(&color(t.accent));
                        b.push_str(")[");
                        b.push_str(&lit(&title.to_uppercase()));
                        b.push_str("]]\n");
                    } else {
                        b.push_str(&widget_markup(
                            &Widget::heading(3, title.clone()),
                            t,
                            assets,
                            full_width,
                        ));
                        b.push('\n');
                    }
                    b.push_str(&inner);
                    b
                } else {
                    inner
                };
                if group.boxed {
                    s.push_str("#block(width: 100%, inset: ");
                    s.push_str(t.block_inset);
                    s.push_str(", radius: 2pt, stroke: 0.5pt + ");
                    s.push_str(&color(t.rule));
                    s.push_str(", above: 0.4em, below: 0.4em)[");
                    s.push_str(&body);
                    s.push(']');
                } else {
                    s.push_str("#block(above: 0.4em, below: 0.4em)[");
                    s.push_str(&body);
                    s.push(']');
                }
                s
            }
            Widget::Divider => {
                format!("#line(length: 100%, stroke: 0.6pt + {})", color(t.rule))
            }
            Widget::Spacer { size } => {
                let v = match size {
                    SpaceSize::Small => "0.4em",
                    SpaceSize::Medium => "1em",
                    SpaceSize::Large => "2em",
                };
                format!("#v({v})")
            }
            Widget::PageBreak => "#pagebreak()".to_string(),
        }
    }

    fn table_markup(table: &Table, t: &Theme, full_width: bool) -> String {
        let mut s = String::new();
        if let Some(title) = &table.title {
            s.push_str("#text(size: ");
            s.push_str(t.h3);
            s.push_str(", weight: \"bold\")[");
            s.push_str(&lit(title));
            s.push_str("]\n#v(0.3em)\n");
        }
        s.push_str("#table(\n  columns: ");
        if full_width {
            // Stretch to the full width. Columns marked `wide` take the
            // slack; failing that, text (Start) columns share it; failing
            // that, the first column stretches. Everything else stays
            // content-sized, so a line-number column stays a line number wide.
            let any_wide = table.columns.iter().any(|c| c.wide);
            let any_start = table.columns.iter().any(|c| c.align == Align::Start);
            s.push('(');
            for (i, c) in table.columns.iter().enumerate() {
                let flex = if any_wide {
                    c.wide
                } else {
                    c.align == Align::Start || (!any_start && i == 0)
                };
                s.push_str(if flex { "1fr, " } else { "auto, " });
            }
            s.push(')');
        } else {
            s.push_str(&table.columns.len().to_string());
        }
        s.push_str(",\n  align: (");
        for c in &table.columns {
            // Cells sit on the baseline of their row: a wrapped description
            // must not drag its own price up with it.
            s.push_str(align_word(c.align));
            s.push_str(" + horizon, ");
        }
        s.push_str("),\n  stroke: ");
        if t.grid {
            s.push_str("0.5pt + ");
            s.push_str(&color(t.rule));
        } else {
            s.push_str("(x, y) => (bottom: 0.5pt + ");
            s.push_str(&color(t.rule));
            s.push(')');
        }
        s.push_str(",\n  inset: ");
        s.push_str(t.cell_inset);
        s.push_str(",\n");
        if t.zebra {
            s.push_str("  fill: (x, y) => if calc.odd(y) { ");
            s.push_str(&color("f9fafb"));
            s.push_str(" },\n");
        }
        // Header row: filled, so it reads as the label band rather than a
        // first line of data. A table whose columns are all unlabelled (a
        // totals block) gets no header at all — an empty filled band is a
        // heading that says nothing.
        if table.columns.iter().any(|c| !c.label.trim().is_empty()) {
            s.push_str("  table.header(");
            for c in &table.columns {
                s.push_str("table.cell(fill: ");
                s.push_str(&color(t.head_fill));
                s.push_str(")[#text(fill: ");
                s.push_str(&color(t.accent));
                s.push_str(", weight: \"bold\", size: ");
                s.push_str(t.small);
                s.push_str(")[");
                s.push_str(&lit(&c.label));
                s.push_str("]], ");
            }
            s.push_str("),\n");
        }
        // Body rows.
        for row in &table.rows {
            s.push_str("  ");
            for cell in row {
                s.push('[');
                s.push_str(&lit(cell));
                s.push_str("], ");
            }
            s.push('\n');
        }
        // Totals as a bold, filled footer row — the line the reader's eye
        // goes to first, so it must not look like one more row of data.
        if let Some(totals) = &table.totals {
            s.push_str("  table.footer(");
            for cell in totals {
                s.push_str("table.cell(fill: ");
                s.push_str(&color(t.head_fill));
                s.push_str(")[#strong[");
                s.push_str(&lit(cell));
                s.push_str("]], ");
            }
            s.push_str("),\n");
        }
        s.push_str(")");
        s
    }

    /// Render a chart to an SVG asset and embed it. The SVG is hand-built
    /// (no chart dependency) and covers bars, grouped/stacked bars, lines,
    /// areas, pies and donuts.
    fn chart_markup(chart: &Chart, t: &Theme, assets: &mut Vec<(String, Vec<u8>)>) -> String {
        let mut s = String::new();
        if let Some(title) = &chart.title {
            s.push_str("#text(size: ");
            s.push_str(t.h3);
            s.push_str(", weight: \"bold\")[");
            s.push_str(&lit(title));
            s.push_str("]\n#v(0.3em)\n");
        }
        let svg = chart_svg(chart);
        let name = format!("chart-{}.svg", assets.len());
        assets.push((name.clone(), svg.into_bytes()));
        s.push_str("#block(width: 100%)[#image(");
        s.push_str(&str_lit(&name));
        s.push_str(", width: 100%, fit: \"contain\")]");
        s
    }

    const PALETTE: [&str; 8] = [
        "2563eb", "16a34a", "f59e0b", "dc2626", "7c3aed", "0891b2", "db2777", "65a30d",
    ];

    fn svg_esc(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    fn chart_svg(chart: &Chart) -> String {
        match chart.kind {
            ChartKind::Pie | ChartKind::Donut => {
                pie_svg(chart, matches!(chart.kind, ChartKind::Donut))
            }
            ChartKind::Line | ChartKind::Area => {
                line_svg(chart, matches!(chart.kind, ChartKind::Area))
            }
            ChartKind::Bar | ChartKind::StackedBar => {
                bar_svg(chart, matches!(chart.kind, ChartKind::StackedBar))
            }
        }
    }

    const W: f64 = 720.0;
    const H: f64 = 320.0;

    fn svg_open() -> String {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {W} {H}\" font-family=\"sans-serif\">"
        )
    }

    fn axis_bounds() -> (f64, f64, f64, f64) {
        // left, top, plot width, plot height
        let (left, top, right, bottom) = (52.0, 16.0, 16.0, 44.0);
        (left, top, W - left - right, H - top - bottom)
    }

    fn text_svg(x: f64, y: f64, size: f64, anchor: &str, fill: &str, s: &str) -> String {
        format!(
            "<text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"{size}\" text-anchor=\"{anchor}\" fill=\"#{fill}\">{}</text>",
            svg_esc(s)
        )
    }

    fn bar_svg(chart: &Chart, stacked: bool) -> String {
        let (left, top, pw, ph) = axis_bounds();
        let baseline = top + ph;
        let cats = chart.labels.len().max(
            chart.series.iter().map(|s| s.values.len()).max().unwrap_or(0),
        );
        let nseries = chart.series.len().max(1);
        let max = if stacked {
            (0..cats)
                .map(|c| chart.series.iter().map(|s| s.values.get(c).copied().unwrap_or(0.0)).sum::<f64>())
                .fold(0.0_f64, f64::max)
        } else {
            chart.series.iter().flat_map(|s| s.values.iter().copied()).fold(0.0_f64, f64::max)
        }
        .max(1.0);

        let mut svg = svg_open();
        // y axis line + baseline
        svg.push_str(&format!(
            "<line x1=\"{left:.1}\" y1=\"{top:.1}\" x2=\"{left:.1}\" y2=\"{baseline:.1}\" stroke=\"#d1d5db\"/>"
        ));
        svg.push_str(&format!(
            "<line x1=\"{left:.1}\" y1=\"{baseline:.1}\" x2=\"{:.1}\" y2=\"{baseline:.1}\" stroke=\"#d1d5db\"/>",
            left + pw
        ));
        svg.push_str(&text_svg(left - 6.0, top + 4.0, 11.0, "end", "6b7280", &format_number(max)));
        svg.push_str(&text_svg(left - 6.0, baseline, 11.0, "end", "6b7280", "0"));

        let group_w = pw / cats as f64;
        for c in 0..cats {
            let gx = left + c as f64 * group_w;
            if stacked {
                let mut acc = 0.0;
                for (si, series) in chart.series.iter().enumerate() {
                    let v = series.values.get(c).copied().unwrap_or(0.0);
                    let h = v / max * ph;
                    let y = baseline - acc - h;
                    let bw = group_w * 0.6;
                    let x = gx + group_w * 0.2;
                    svg.push_str(&format!(
                        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{bw:.1}\" height=\"{h:.1}\" fill=\"#{}\"/>",
                        PALETTE[si % PALETTE.len()]
                    ));
                    acc += h;
                }
            } else {
                let slot = group_w * 0.7 / nseries as f64;
                for (si, series) in chart.series.iter().enumerate() {
                    let v = series.values.get(c).copied().unwrap_or(0.0);
                    let h = v / max * ph;
                    let x = gx + group_w * 0.15 + si as f64 * slot;
                    let y = baseline - h;
                    svg.push_str(&format!(
                        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{h:.1}\" rx=\"2\" fill=\"#{}\"/>",
                        slot * 0.85,
                        PALETTE[si % PALETTE.len()]
                    ));
                }
            }
            if let Some(label) = chart.labels.get(c) {
                svg.push_str(&text_svg(gx + group_w / 2.0, baseline + 16.0, 11.0, "middle", "374151", label));
            }
        }
        svg.push_str(&legend(chart));
        svg.push_str("</svg>");
        svg
    }

    fn line_svg(chart: &Chart, area: bool) -> String {
        let (left, top, pw, ph) = axis_bounds();
        let baseline = top + ph;
        let max = chart
            .series
            .iter()
            .flat_map(|s| s.values.iter().copied())
            .fold(0.0_f64, f64::max)
            .max(1.0);
        let mut svg = svg_open();
        svg.push_str(&format!(
            "<line x1=\"{left:.1}\" y1=\"{top:.1}\" x2=\"{left:.1}\" y2=\"{baseline:.1}\" stroke=\"#d1d5db\"/>"
        ));
        svg.push_str(&format!(
            "<line x1=\"{left:.1}\" y1=\"{baseline:.1}\" x2=\"{:.1}\" y2=\"{baseline:.1}\" stroke=\"#d1d5db\"/>",
            left + pw
        ));
        svg.push_str(&text_svg(left - 6.0, top + 4.0, 11.0, "end", "6b7280", &format_number(max)));

        for (si, series) in chart.series.iter().enumerate() {
            let n = series.values.len().max(1);
            let step = if n > 1 { pw / (n as f64 - 1.0) } else { pw };
            let color = PALETTE[si % PALETTE.len()];
            let pts: Vec<(f64, f64)> = series
                .values
                .iter()
                .enumerate()
                .map(|(i, v)| (left + i as f64 * step, baseline - v / max * ph))
                .collect();
            if area {
                let mut d = format!("M {:.1} {:.1}", left, baseline);
                for (x, y) in &pts {
                    d.push_str(&format!(" L {x:.1} {y:.1}"));
                }
                d.push_str(&format!(" L {:.1} {:.1} Z", left + (n as f64 - 1.0) * step, baseline));
                svg.push_str(&format!("<path d=\"{d}\" fill=\"#{color}\" fill-opacity=\"0.15\"/>"));
            }
            let poly: String = pts.iter().map(|(x, y)| format!("{x:.1},{y:.1} ")).collect();
            svg.push_str(&format!(
                "<polyline points=\"{poly}\" fill=\"none\" stroke=\"#{color}\" stroke-width=\"2\"/>"
            ));
            for (x, y) in &pts {
                svg.push_str(&format!("<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"3\" fill=\"#{color}\"/>"));
            }
        }
        for (i, label) in chart.labels.iter().enumerate() {
            let n = chart.labels.len().max(1);
            let step = if n > 1 { pw / (n as f64 - 1.0) } else { pw };
            svg.push_str(&text_svg(left + i as f64 * step, baseline + 16.0, 11.0, "middle", "374151", label));
        }
        svg.push_str(&legend(chart));
        svg.push_str("</svg>");
        svg
    }

    fn pie_svg(chart: &Chart, donut: bool) -> String {
        let values = chart.series.first().map(|s| s.values.clone()).unwrap_or_default();
        let total: f64 = values.iter().sum();
        let (cx, cy, r) = (170.0, H / 2.0, 120.0);
        let mut svg = svg_open();
        if total <= 0.0 {
            svg.push_str(&text_svg(cx, cy, 12.0, "middle", "6b7280", "No data"));
            svg.push_str("</svg>");
            return svg;
        }
        let mut angle = -std::f64::consts::FRAC_PI_2;
        for (i, v) in values.iter().enumerate() {
            let sweep = v / total * std::f64::consts::TAU;
            let a1 = angle + sweep;
            let (x0, y0) = (cx + r * angle.cos(), cy + r * angle.sin());
            let (x1, y1) = (cx + r * a1.cos(), cy + r * a1.sin());
            let large = if sweep > std::f64::consts::PI { 1 } else { 0 };
            svg.push_str(&format!(
                "<path d=\"M {cx:.1} {cy:.1} L {x0:.1} {y0:.1} A {r:.1} {r:.1} 0 {large} 1 {x1:.1} {y1:.1} Z\" fill=\"#{}\"/>",
                PALETTE[i % PALETTE.len()]
            ));
            angle = a1;
        }
        if donut {
            svg.push_str(&format!(
                "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{:.1}\" fill=\"#ffffff\"/>",
                r * 0.55
            ));
        }
        // Legend with labels + values to the right.
        let lx = cx + r + 40.0;
        let mut ly = cy - (values.len() as f64 * 22.0) / 2.0 + 8.0;
        for (i, v) in values.iter().enumerate() {
            let label = chart.labels.get(i).cloned().unwrap_or_default();
            let pct = (v / total * 100.0).round() as i64;
            svg.push_str(&format!(
                "<rect x=\"{lx:.1}\" y=\"{:.1}\" width=\"12\" height=\"12\" rx=\"2\" fill=\"#{}\"/>",
                ly - 10.0,
                PALETTE[i % PALETTE.len()]
            ));
            svg.push_str(&text_svg(
                lx + 18.0,
                ly,
                12.0,
                "start",
                "374151",
                &format!("{label} — {} ({pct}%)", format_number(*v)),
            ));
            ly += 22.0;
        }
        svg.push_str("</svg>");
        svg
    }

    /// A horizontal legend along the bottom for multi-series cartesian charts.
    fn legend(chart: &Chart) -> String {
        if chart.series.len() < 2 {
            return String::new();
        }
        let mut x = 60.0;
        let y = H - 6.0;
        let mut svg = String::new();
        for (i, series) in chart.series.iter().enumerate() {
            svg.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{:.1}\" width=\"12\" height=\"12\" rx=\"2\" fill=\"#{}\"/>",
                y - 10.0,
                PALETTE[i % PALETTE.len()]
            ));
            svg.push_str(&text_svg(x + 16.0, y, 11.0, "start", "374151", &series.name));
            x += 32.0 + series.name.len() as f64 * 6.5;
        }
        svg
    }

    fn placeholder_box(kind: &str, data: &str, caption: Option<&str>, t: &Theme) -> String {
        let mut s = String::from("#block(inset: 8pt, radius: 4pt, stroke: 0.6pt + ");
        s.push_str(&color(t.rule));
        s.push_str(")[#text(size: ");
        s.push_str(t.small);
        s.push_str(", fill: ");
        s.push_str(&color(t.muted));
        s.push_str(")[");
        s.push_str(&lit(&format!("[{kind}] {data}")));
        s.push(']');
        if let Some(cap) = caption {
            s.push_str(" \\ #text(size: ");
            s.push_str(t.small);
            s.push_str(")[");
            s.push_str(&lit(cap));
            s.push(']');
        }
        s.push(']');
        s
    }

    fn format_number(v: f64) -> String {
        if v.fract() == 0.0 {
            format!("{}", v as i64)
        } else {
            format!("{v:.1}")
        }
    }

    /// A safe file extension for a virtual asset name.
    fn sanitize_ext(ext: &str) -> String {
        let clean: String = ext
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(5)
            .collect::<String>()
            .to_ascii_lowercase();
        if clean.is_empty() { "png".into() } else { clean }
    }
}

/// The Excel backend: every [`Table`] in the document becomes a worksheet
/// (headers and totals bold). Non-table widgets are skipped — Excel is for
/// list/tabular data, which is why the engine only offers it for reports
/// that opt in.
#[cfg(feature = "reporting")]
mod excel_backend {
    use super::*;
    use rust_xlsxwriter::{Format, Workbook, XlsxError};

    pub(super) struct XlsxRenderer;

    impl XlsxRenderer {
        pub(super) fn new() -> Self {
            Self
        }
    }

    impl ReportRenderer for XlsxRenderer {
        fn render(&self, doc: &Document) -> Result<Rendered> {
            let mut workbook = Workbook::new();
            let bold = Format::new().set_bold();

            let mut tables = Vec::new();
            collect_tables(&doc.report.widgets, &mut tables);

            if tables.is_empty() {
                let ws = workbook.add_worksheet();
                ws.write(0, 0, doc.report.title.as_str()).map_err(xerr)?;
                ws.write(1, 0, "This report has no tabular data to export to Excel.")
                    .map_err(xerr)?;
            } else {
                for (i, table) in tables.iter().enumerate() {
                    let ws = workbook.add_worksheet();
                    let _ = ws.set_name(sheet_name(table.title.as_deref(), i));
                    let mut row = 0u32;
                    for (c, col) in table.columns.iter().enumerate() {
                        ws.write_with_format(row, c as u16, col.label.as_str(), &bold)
                            .map_err(xerr)?;
                    }
                    row += 1;
                    for r in &table.rows {
                        for (c, cell) in r.iter().enumerate() {
                            ws.write(row, c as u16, cell.as_str()).map_err(xerr)?;
                        }
                        row += 1;
                    }
                    if let Some(totals) = &table.totals {
                        for (c, cell) in totals.iter().enumerate() {
                            ws.write_with_format(row, c as u16, cell.as_str(), &bold)
                                .map_err(xerr)?;
                        }
                    }
                    let _ = ws.autofit();
                }
            }

            let bytes = workbook.save_to_buffer().map_err(xerr)?;
            Ok(Rendered {
                bytes,
                content_type:
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                extension: "xlsx",
                file_name: None,
            })
        }
    }

    /// A worksheet name Excel accepts: no `\ / ? * [ ] :`, at most 31 chars.
    fn sheet_name(title: Option<&str>, index: usize) -> String {
        let base = title.unwrap_or("Sheet");
        let clean: String = base
            .chars()
            .filter(|c| !matches!(c, '\\' | '/' | '?' | '*' | '[' | ']' | ':'))
            .take(28)
            .collect();
        let clean = clean.trim();
        if clean.is_empty() {
            format!("Sheet{}", index + 1)
        } else {
            clean.to_string()
        }
    }

    fn xerr(e: XlsxError) -> Error {
        Error::internal(format!("Excel export failed: {e}"))
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

/// Decode standard base64 (padded), ignoring whitespace. Returns `None` on
/// malformed input.
#[cfg(feature = "reporting")]
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in text.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
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

    #[cfg(feature = "reporting")]
    #[test]
    fn base64_round_trips() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foobar", &[0u8, 255, 16, 32, 64]] {
            assert_eq!(base64_decode(&base64_encode(sample)).unwrap(), sample);
        }
    }

    #[test]
    fn formats_and_outputs_parse() {
        assert_eq!(ReportFormat::parse("Compact"), Some(ReportFormat::Compact));
        assert_eq!(ReportFormat::parse("nope"), None);
        assert_eq!(ReportOutput::parse("xlsx"), Some(ReportOutput::Excel));
    }

    /// The per-theme letterhead must typeset (and export SVG) for every
    /// format with a fully populated company profile — the new contact
    /// fields and the distinct header layouts all produce valid Typst.
    #[cfg(feature = "reporting")]
    #[test]
    fn letterhead_renders_for_every_format() {
        let company = CompanyInformation {
            name: "Acme Manufacturing Ltd".into(),
            address: Some("12 Industrial Way\nNairobi 00100\nKenya".into()),
            email: Some("hello@acme.example".into()),
            website: Some("www.acme.example".into()),
            phone: Some("+254 700 000 000".into()),
            tax_pin: Some("P051234567X".into()),
            vat_number: Some("0192837465".into()),
            currency: Some("KES".into()),
            logo: None,
        };
        for format in [
            ReportFormat::Modern,
            ReportFormat::Compact,
            ReportFormat::Corporate,
        ] {
            let doc = Document {
                company: company.clone(),
                report: Report::new("Quarterly Summary").subtitle("Q2 2026"),
                title: "Quarterly Summary".into(),
                format,
                watermark: Some("DRAFT".into()),
            };
            let pdf = typst_backend::TypstRenderer::new()
                .render(&doc)
                .unwrap_or_else(|e| panic!("{format:?} header PDF failed: {e}"));
            assert!(pdf.bytes.starts_with(b"%PDF"), "{format:?} is not a PDF");
            let pages = typst_backend::render_svg(&doc)
                .unwrap_or_else(|e| panic!("{format:?} header SVG failed: {e}"));
            assert!(
                pages.first().is_some_and(|p| p.trim_start().starts_with("<svg")),
                "{format:?} preview is not SVG"
            );
            if let Ok(dir) = std::env::var("REPORT_OUT_DIR") {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(
                    format!("{dir}/letterhead-{}.svg", format.as_str()),
                    pages[0].as_bytes(),
                );
                let _ = std::fs::write(
                    format!("{dir}/letterhead-{}.pdf", format.as_str()),
                    &pdf.bytes,
                );
            }
        }
    }

    /// A tenant that has uploaded no logo gets a letterhead of its name and
    /// contacts — never an invented mark. The company's stationery must not
    /// carry a graphic the company never chose.
    #[cfg(feature = "reporting")]
    #[test]
    fn no_logo_means_no_brand_mark() {
        let company = CompanyInformation {
            name: "Acme Manufacturing Ltd".into(),
            logo: None,
            ..Default::default()
        };
        for format in [
            ReportFormat::Modern,
            ReportFormat::Compact,
            ReportFormat::Corporate,
        ] {
            let mut assets = Vec::new();
            let doc = Document {
                company: company.clone(),
                report: Report::new("Quarterly Summary"),
                title: "Quarterly Summary".into(),
                format,
                watermark: None,
            };
            let markup = typst_backend::emit(&doc, &mut assets);
            assert!(
                assets.is_empty(),
                "{format:?} registered an image asset with no logo set"
            );
            assert!(
                !markup.contains("#image("),
                "{format:?} drew an image with no logo set"
            );
            // The old monogram tile: initials reversed out of an accent square.
            assert!(
                !markup.contains("AM"),
                "{format:?} still draws a monogram fallback"
            );
            // The name itself must survive — that is the letterhead now.
            assert!(
                markup.contains("Acme Manufacturing Ltd"),
                "{format:?} dropped the company name"
            );
        }
    }

    /// The logo is embedded when the tenant has one — the counterpart to
    /// [`no_logo_means_no_brand_mark`], so "no mark" cannot pass by drawing
    /// nothing ever.
    #[cfg(feature = "reporting")]
    #[test]
    fn a_set_logo_is_embedded() {
        // A 1×1 PNG.
        let png = base64_encode(&[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ]);
        let company = CompanyInformation {
            name: "Acme Manufacturing Ltd".into(),
            logo: Some(LogoImage { format: "png".into(), data_base64: png }),
            ..Default::default()
        };
        let mut assets = Vec::new();
        let doc = Document {
            company,
            report: Report::new("Quarterly Summary"),
            title: "Quarterly Summary".into(),
            format: ReportFormat::Modern,
            watermark: None,
        };
        let markup = typst_backend::emit(&doc, &mut assets);
        assert!(markup.contains("#image("), "a set logo was not drawn");
        assert_eq!(assets.len(), 1, "the logo bytes were not registered");
        assert_eq!(assets[0].0, "logo.png");
    }

    /// A list export is a report like any other: it carries the title once,
    /// on the page, and the table beneath it adds no heading of its own.
    #[test]
    fn an_exported_list_titles_itself_once() {
        let report = Report::new("Purchase Orders")
            .subtitle("Status: confirmed · 2 records")
            .with(
                Table::new(vec![Column::new("Number"), Column::number("Total")])
                    .row(["PO-2026-00001", "1,240.00"])
                    .into_widget(),
            );
        let Widget::Table(table) = &report.widgets[0] else {
            panic!("expected a table widget");
        };
        assert_eq!(table.title, None, "the table repeats the report's title");
        assert_eq!(report.title, "Purchase Orders");
    }

    fn params(pairs: &[(&str, &str)]) -> ReportParams {
        ReportParams::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    /// A malformed argument must name itself. A report asked for the wrong
    /// thing should say which parameter was wrong, not fail as a bare parse
    /// error the caller cannot act on.
    #[test]
    fn a_bad_parameter_names_itself() {
        let p = params(&[("id", "not-a-uuid"), ("from", "13/01/2026")]);

        let err = p.id().unwrap_err().to_string();
        assert!(err.contains("\"id\""), "{err}");

        let err = p.date("from").unwrap_err().to_string();
        assert!(err.contains("\"from\""), "{err}");

        let err = params(&[]).id().unwrap_err().to_string();
        assert!(err.contains("\"id\""), "{err}");
    }

    /// A blank value is not an answer: `?from=` in a URL means the caller
    /// left the box empty, which must read as unset rather than as a date
    /// that fails to parse.
    #[test]
    fn blank_parameters_read_as_unset() {
        let p = params(&[("from", ""), ("to", "   "), ("id", "x")]);
        assert!(p.get("from").is_none());
        assert!(p.date("to").unwrap().is_none());
        assert_eq!(p.get("id"), Some("x"));
    }

    #[test]
    fn parameters_parse_what_reports_ask_for() {
        let p = params(&[
            ("id", "3f2504e0-4f89-11d3-9a0c-0305e82c3301"),
            ("from", "2026-01-01"),
        ]);
        assert_eq!(p.id().unwrap().to_string(), "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
        assert_eq!(
            p.date("from").unwrap(),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap())
        );
        assert!(p.date("to").unwrap().is_none());
        assert!(params(&[]).is_empty());
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
