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

    // Render the overview: default format (modern) → a real PDF.
    let rendered = reporting
        .render(&render_cx(&app), "workspace-overview", None, ReportOutput::Pdf)
        .await
        .map_err(|e| format!("overview render failed: {e}"))?;
    assert_pdf(&rendered.bytes, "overview (modern)")?;
    dump("workspace-overview.modern", &rendered.bytes);

    // The engine reports the report catalogue.
    if reporting.required_permission("workspace-overview").is_some() {
        return Err("overview should be open (no permission declared)".into());
    }

    // Set tenant house format + watermark; the engine must read them back
    // and every format must still typeset.
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
    let settings = reporting.settings(app.database()).await;
    if settings.default_format != Some(ReportFormat::Compact) || settings.watermark.as_deref() != Some("DRAFT") {
        return Err(format!("settings did not persist: {settings:?}"));
    }

    // House default (compact) + watermark path.
    let rendered = reporting
        .render(&render_cx(&app), "workspace-overview", None, ReportOutput::Pdf)
        .await
        .map_err(|e| format!("compact render failed: {e}"))?;
    assert_pdf(&rendered.bytes, "overview (compact + watermark)")?;
    dump("workspace-overview.compact-watermarked", &rendered.bytes);

    // An explicit format still overrides the house default.
    let rendered = reporting
        .render(
            &render_cx(&app),
            "workspace-overview",
            Some(ReportFormat::Corporate),
            ReportOutput::Pdf,
        )
        .await
        .map_err(|e| format!("corporate render failed: {e}"))?;
    assert_pdf(&rendered.bytes, "overview (corporate)")?;
    dump("workspace-overview.corporate", &rendered.bytes);

    // The list report renders to PDF and to Excel.
    let rendered = reporting
        .render(&render_cx(&app), "sample-register", None, ReportOutput::Pdf)
        .await
        .map_err(|e| format!("register pdf failed: {e}"))?;
    assert_pdf(&rendered.bytes, "register (pdf)")?;
    dump("sample-register.pdf", &rendered.bytes);
    let xlsx = reporting
        .render(&render_cx(&app), "sample-register", None, ReportOutput::Excel)
        .await
        .map_err(|e| format!("register excel failed: {e}"))?;
    if xlsx.bytes.len() < 200 || &xlsx.bytes[..2] != b"PK" {
        return Err("register Excel is not a valid xlsx (missing ZIP magic)".into());
    }
    if xlsx.extension != "xlsx" {
        return Err(format!("expected xlsx extension, got {}", xlsx.extension));
    }

    // The themed in-app preview: one SVG per page, matching the PDF layout.
    let pages = reporting
        .preview(&render_cx(&app), "workspace-overview", Some(ReportFormat::Modern))
        .await
        .map_err(|e| format!("overview preview failed: {e}"))?;
    if pages.is_empty() {
        return Err("preview produced no pages".into());
    }
    for (i, page) in pages.iter().enumerate() {
        if !page.trim_start().starts_with("<svg") {
            return Err(format!("preview page {i} is not SVG"));
        }
    }
    dump_text("workspace-overview.preview.p1", "svg", pages[0].as_bytes());

    // The interactive datatable output: the register's table, flattened with
    // per-column hints for the viewer.
    let tables = reporting
        .datatables(&render_cx(&app), "sample-register", None)
        .await
        .map_err(|e| format!("register datatables failed: {e}"))?;
    if tables.tables.len() != 1 {
        return Err(format!("expected 1 datatable, got {}", tables.tables.len()));
    }
    let table = &tables.tables[0];
    if table.rows.is_empty() || table.columns.len() != 5 {
        return Err(format!(
            "unexpected datatable shape: {} cols, {} rows",
            table.columns.len(),
            table.rows.len()
        ));
    }
    // The "Amount" column is right-aligned, so it must be flagged numeric.
    if !table.columns[3].numeric {
        return Err("Amount column should be numeric".into());
    }

    // The overview declares no table output, so datatables must reject it.
    if reporting
        .datatables(&render_cx(&app), "workspace-overview", None)
        .await
        .is_ok()
    {
        return Err("overview should reject the table output".into());
    }

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

fn assert_pdf(bytes: &[u8], what: &str) -> Result<(), String> {
    if bytes.len() < 800 {
        return Err(format!("{what}: PDF too small ({} bytes)", bytes.len()));
    }
    if &bytes[..5] != b"%PDF-" {
        return Err(format!("{what}: not a PDF"));
    }
    Ok(())
}

/// When REPORT_OUT_DIR is set, write the rendered PDFs there for eyeballing.
fn dump(name: &str, bytes: &[u8]) {
    if let Ok(dir) = std::env::var("REPORT_OUT_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(format!("{dir}/{name}.pdf"), bytes);
    }
}

/// Like [`dump`], but for text artefacts (SVG preview pages).
fn dump_text(name: &str, ext: &str, bytes: &[u8]) {
    if let Ok(dir) = std::env::var("REPORT_OUT_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(format!("{dir}/{name}.{ext}"), bytes);
    }
}
