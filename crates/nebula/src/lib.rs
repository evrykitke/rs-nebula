//! Nebula — a DDD application framework for building ERPs in Rust.
//!
//! Inspired by ASP.NET Boilerplate: applications are composed from modules,
//! bootstrapped by a kernel, and configured rather than hardcoded.

pub mod account;
pub mod administration;
pub mod audit;
pub mod auth;
pub mod cache;
pub mod config;
pub mod crypto;
pub mod dashboard;
pub mod db;
pub mod error;
pub mod events;
pub mod jobs;
pub mod kernel;
pub mod logging;
pub mod mail;
pub mod migrations;
pub mod module;
pub mod money;
pub mod numbering;
pub mod ports;
pub mod reporting;
pub mod repository;
pub mod sql_migrations;
pub mod storage;
pub mod tenancy;
pub mod time;
mod web;

/// Exact decimal arithmetic for money and quantities — never `f64`,
/// which cannot represent amounts like 0.1 exactly and accumulates
/// rounding errors.
pub use rust_decimal::Decimal;

/// Re-exported so applications use the same SeaORM the framework links.
pub use sea_orm;
pub use sea_orm_migration;

/// Re-exported so applications build workers against the same apalis.
pub use apalis;

pub use jobs::Jobs;
pub use account::AccountModule;
pub use administration::AdministrationModule;
pub use auth::{CurrentUser as AuthUser, UserManager};
pub use cache::{Cache, Scope as CacheScope};
pub use config::Config;
pub use crypto::Cipher;
pub use dashboard::{
    ChartData, ChartType, DashboardView, Dashboards, ListData, ListItemData, PlacedWidget,
    PlacedWidgetView, ProgressData, ProgressItemData, SeriesData, StatData, TableColumnData,
    TableData, TrendDirection, WidgetCx, WidgetData, WidgetDefinition, WidgetInfo, WidgetKind,
};
pub use error::{Error, Result};
pub use events::{Event, Events};
pub use kernel::{App, Kernel};
pub use mail::{Mailer, Message as MailMessage};
pub use module::{Module, ModuleContext};
pub use money::{Currency, CurrencyRegistry, Money};
pub use numbering::{Number, Numbering, NumberingHandle, Reset, SeriesDef};
pub use reporting::{
    Align, Callout, CalloutStyle, Chart, ChartKind, Column, CompanyInformation, DataColumn, DataCx,
    DataTable, Group, Image, KeyValue, Metric, Orientation, Progress, Report, ReportData,
    ReportDataSource, ReportDefinition, ReportFormat, ReportInfo, ReportJob, ReportJobStatus,
    ReportOutput, ReportSettings, ReportTables, Reporting, Row, RowTone, Series, Signature,
    SpaceSize, Symbology, Table, TextStyle, Trend, Widget,
};
pub use repository::Repository;
pub use sql_migrations::SqlMigrator;
pub use storage::{Container, Storage, StoredFile};
pub use tenancy::middleware::{CurrentTenant, TenantDb};
pub use tenancy::{TenantManager, TenantRef};
pub use time::{Clock, SystemClock};