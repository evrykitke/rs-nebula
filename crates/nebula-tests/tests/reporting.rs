//! Proof of concept: the reporting engine end to end against a live
//! database. A declared report resolves the company datasource, builds its
//! widget tree, and renders; tenant report settings (house format +
//! watermark) are persisted and applied. Uses a throwaway database so it
//! never touches dev data. Skips when NEBULA_TEST_DATABASE_URL is unset.

use nebula::config::{Config, DatabaseConfig};
use nebula::reporting::{RenderCx, ReportFormat, ReportOutput, ReportSettings};
use nebula::{App, Kernel, db};
use nebula_apps::WorkspaceApp;
use sea_orm::ConnectionTrait;

async fn boot(url: &str) -> App {
    let mut config = Config::default();
    config.auth.jwt_secret = "reporting-test-secret".into();
    config.database = DatabaseConfig {
        url: url.into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    Kernel::builder()
        .with_config(config)
        .add_module(WorkspaceApp)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("app must boot")
}

fn render_cx(app: &App) -> RenderCx {
    RenderCx {
        db: app.database().cloned(),
        tenant: None,
    }
}

#[tokio::test]
async fn renders_reports_and_applies_tenant_settings() {
    let Ok(main_url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let admin = db::connect(&DatabaseConfig {
        url: main_url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to create the test database");

    let fresh = format!("nebula_report_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute_unprepared(&format!("CREATE DATABASE {fresh}"))
        .await
        .expect("must create the fresh database");

    let outcome = run(&swap_database(&main_url, &fresh)).await;

    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {fresh} WITH (FORCE)"))
        .await;

    outcome.expect("reporting flow must pass");
}

async fn run(url: &str) -> Result<(), String> {
    let app = boot(url).await;
    let reporting = app.reporting();

    // Two reports were declared by the workspace app.
    let mut names = reporting.names();
    names.sort();
    if names != ["sample-register", "workspace-overview"] {
        return Err(format!("unexpected report names: {names:?}"));
    }

    // Render the overview: default format (modern), a body of widgets.
    let rendered = reporting
        .render(&render_cx(&app), "workspace-overview", None, ReportOutput::Pdf)
        .await
        .map_err(|e| format!("overview render failed: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&rendered.bytes).map_err(|e| e.to_string())?;
    if doc["format"] != "modern" {
        return Err(format!("expected modern default, got {}", doc["format"]));
    }
    if doc["title"] != "Workspace Overview" {
        return Err("wrong title".into());
    }
    let widgets = doc["report"]["widgets"].as_array().ok_or("no widgets")?;
    if widgets.len() < 8 {
        return Err(format!("expected a rich widget tour, got {}", widgets.len()));
    }

    // Set tenant house format + watermark; both must apply on the next render.
    reporting
        .save_settings(
            app.database().unwrap(),
            &ReportSettings {
                default_format: Some(ReportFormat::Compact),
                watermark: Some("DRAFT".into()),
            },
        )
        .await
        .map_err(|e| format!("save_settings failed: {e}"))?;

    let rendered = reporting
        .render(&render_cx(&app), "workspace-overview", None, ReportOutput::Pdf)
        .await
        .map_err(|e| format!("second render failed: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&rendered.bytes).map_err(|e| e.to_string())?;
    if doc["format"] != "compact" {
        return Err(format!("house format not applied: {}", doc["format"]));
    }
    if doc["watermark"] != "DRAFT" {
        return Err(format!("watermark not applied: {}", doc["watermark"]));
    }

    // An explicit format still overrides the house default.
    let rendered = reporting
        .render(
            &render_cx(&app),
            "workspace-overview",
            Some(ReportFormat::Corporate),
            ReportOutput::Pdf,
        )
        .await
        .map_err(|e| format!("explicit-format render failed: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&rendered.bytes).map_err(|e| e.to_string())?;
    if doc["format"] != "corporate" {
        return Err(format!("explicit format ignored: {}", doc["format"]));
    }

    // The list report supports Excel.
    reporting
        .render(&render_cx(&app), "sample-register", None, ReportOutput::Excel)
        .await
        .map_err(|e| format!("register excel failed: {e}"))?;

    // The overview does not support Excel.
    if reporting
        .render(&render_cx(&app), "workspace-overview", None, ReportOutput::Excel)
        .await
        .is_ok()
    {
        return Err("overview should reject Excel".into());
    }

    // An unknown report is a not-found.
    if reporting
        .render(&render_cx(&app), "nope", None, ReportOutput::Pdf)
        .await
        .is_ok()
    {
        return Err("unknown report should fail".into());
    }

    Ok(())
}

fn swap_database(url: &str, database: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database}"),
        None => format!("{url}/{database}"),
    }
}
