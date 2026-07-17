//! Proof of concept: the reporting engine end to end against a live
//! database. A declared report resolves the company datasource, builds its
//! widget tree, and renders; tenant report settings (house format +
//! watermark) are persisted and applied. Uses a throwaway database so it
//! never touches dev data. Skips when NEBULA_TEST_DATABASE_URL is unset.

use nebula::config::{Config, DatabaseConfig};
use nebula::reporting::{RenderCx, ReportFormat, ReportJobStatus, ReportOutput, ReportSettings};
use nebula::{App, Kernel, db};
use nebula_apps::WorkspaceApp;
use sea_orm::ConnectionTrait;
use std::time::Duration;

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
    RenderCx::new(app.database().cloned(), None)
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

/// Data-heavy reports can be queued: a worker renders off-request, stores
/// the artifact and settles the job row. Needs Postgres and Redis.
#[tokio::test]
async fn queues_and_renders_reports_in_background() {
    let Ok(main_url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };
    let redis_url =
        std::env::var("NEBULA_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    if !redis_reachable(&redis_url) {
        eprintln!("SKIPPED: Redis not reachable at {redis_url} (is docker compose up?)");
        return;
    }

    let admin = db::connect(&DatabaseConfig {
        url: main_url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to create the test database");

    let fresh = format!("nebula_report_jobs_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute_unprepared(&format!("CREATE DATABASE {fresh}"))
        .await
        .expect("must create the fresh database");

    let outcome = run_jobs(&swap_database(&main_url, &fresh), &redis_url).await;

    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {fresh} WITH (FORCE)"))
        .await;

    outcome.expect("background report flow must pass");
}

async fn run_jobs(url: &str, redis_url: &str) -> Result<(), String> {
    let files_root = std::env::temp_dir().join(format!(
        "nebula-report-artifacts-{}",
        uuid::Uuid::new_v4().simple()
    ));

    let mut config = Config::default();
    config.auth.jwt_secret = "reporting-jobs-test-secret".into();
    config.database = DatabaseConfig {
        url: url.into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.jobs.enabled = true;
    config.redis.url = redis_url.into();
    config.files.root = files_root.to_string_lossy().into_owned();
    let private_root = files_root.join("_private");
    config.files.private_root = private_root.to_string_lossy().into_owned();

    let mut app = Kernel::builder()
        .with_config(config)
        .add_module(WorkspaceApp)
        .build()
        .map_err(|e| format!("kernel build failed: {e}"))?
        .init()
        .await
        .map_err(|e| format!("boot failed: {e}"))?;

    let reporting = app.reporting();
    let db = app.database().cloned();
    let cx = RenderCx::new(db.clone(), None);
    let jobs = app.jobs().ok_or("jobs client must exist")?;

    // An unsupported output is rejected synchronously, before anything queues.
    if reporting
        .enqueue_job(&cx, &jobs, "workspace-overview", None, ReportOutput::Excel, None)
        .await
        .is_ok()
    {
        return Err("overview should reject an Excel job".into());
    }

    // Queue a real render; it starts life queued.
    let job = reporting
        .enqueue_job(
            &cx,
            &jobs,
            "sample-register",
            None,
            ReportOutput::Pdf,
            Some((uuid::Uuid::new_v4(), "tester".into())),
        )
        .await
        .map_err(|e| format!("enqueue failed: {e}"))?;
    if job.status != ReportJobStatus::Queued {
        return Err(format!("a new job should be queued, was {:?}", job.status));
    }

    // Run the workers and wait for this job to settle.
    if !app.start_jobs() {
        return Err("job monitor must start".into());
    }
    let mut waited = Duration::ZERO;
    let finished = loop {
        let current = reporting
            .job(db.as_ref(), job.id)
            .await
            .map_err(|e| format!("job poll failed: {e}"))?;
        if matches!(
            current.status,
            ReportJobStatus::Completed | ReportJobStatus::Failed
        ) {
            break current;
        }
        if waited >= Duration::from_secs(30) {
            return Err(format!("job did not finish in time (last: {:?})", current.status));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        waited += Duration::from_millis(250);
    };

    if finished.status != ReportJobStatus::Completed {
        return Err(format!("job failed: {:?}", finished.error));
    }
    if finished.content_type.as_deref() != Some("application/pdf") {
        return Err(format!("unexpected content type: {:?}", finished.content_type));
    }
    if finished.byte_size.unwrap_or(0) < 800 {
        return Err(format!("artifact too small: {:?}", finished.byte_size));
    }
    if !finished.file_name.as_deref().unwrap_or("").ends_with(".pdf") {
        return Err(format!("unexpected file name: {:?}", finished.file_name));
    }

    // The artifact landed in the PRIVATE store (financial data must never
    // sit under the unauthenticated /public root) and is a PDF of the
    // recorded size.
    let stored =
        find_artifact(&private_root, ".pdf").ok_or("no stored artifact in the private root")?;
    let bytes = std::fs::read(&stored).map_err(|e| format!("read artifact: {e}"))?;
    if !bytes.starts_with(b"%PDF-") {
        return Err("stored artifact is not a PDF".into());
    }
    if bytes.len() as i64 != finished.byte_size.unwrap_or(-1) {
        return Err("stored size disagrees with the recorded byte_size".into());
    }
    // ...and the served /public tree holds no copy of it. (The private root
    // nests under files_root here purely for test cleanup; in a deployment
    // they are siblings. Skip the nested private dir when sweeping.)
    for ns in std::fs::read_dir(&files_root).map_err(|e| e.to_string())?.flatten() {
        if ns.path() == private_root {
            continue;
        }
        if find_artifact(&ns.path(), ".pdf").is_some() || ns.path().to_string_lossy().ends_with(".pdf") {
            return Err("a report artifact leaked into the public files root".into());
        }
    }

    // It shows up in the job history (checked before the retention sweep
    // below deletes it).
    let history = reporting
        .jobs(db.as_ref(), 10)
        .await
        .map_err(|e| format!("history failed: {e}"))?;
    if !history.iter().any(|j| j.id == job.id) {
        return Err("completed job missing from history".into());
    }

    // The retention sweep removes expired jobs and their files: with a
    // cutoff in the future everything just stored is "old".
    let pruned = reporting
        .prune_jobs(db.as_ref(), 1)
        .await
        .map_err(|e| format!("prune failed: {e}"))?;
    if pruned != 0 {
        return Err("nothing should be pruned inside the retention window".into());
    }
    sea_orm::ConnectionTrait::execute_unprepared(
        db.as_ref().ok_or("db must exist")?,
        "UPDATE report_jobs SET created_at = created_at - INTERVAL '10 days'",
    )
    .await
    .map_err(|e| format!("backdating jobs failed: {e}"))?;
    let pruned = reporting
        .prune_jobs(db.as_ref(), 7)
        .await
        .map_err(|e| format!("prune failed: {e}"))?;
    if pruned == 0 {
        return Err("backdated jobs should have been pruned".into());
    }
    if find_artifact(&private_root, ".pdf").is_some() {
        return Err("the pruned artifact should be gone from disk".into());
    }
    if reporting.job(db.as_ref(), job.id).await.is_ok() {
        return Err("the pruned job row should be gone".into());
    }

    let _ = std::fs::remove_dir_all(&files_root);
    Ok(())
}

/// A quick TCP probe so the test skips (rather than fails) when Redis is
/// not running locally.
fn redis_reachable(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let hostport = url
        .trim_start_matches("redis://")
        .trim_start_matches("rediss://");
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    let hostport = hostport.rsplit('@').next().unwrap_or(hostport);
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:6379")
    };
    match addr.to_socket_addrs() {
        Ok(mut addrs) => addrs.next().is_some_and(|a| {
            std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok()
        }),
        Err(_) => false,
    }
}

/// Find the first file ending with `ext` under the `{root}/{ns}/{id}/`
/// storage layout.
fn find_artifact(root: &std::path::Path, ext: &str) -> Option<std::path::PathBuf> {
    for ns in std::fs::read_dir(root).ok()?.flatten() {
        if !ns.path().is_dir() {
            continue;
        }
        for id in std::fs::read_dir(ns.path()).ok()?.flatten() {
            if !id.path().is_dir() {
                continue;
            }
            for file in std::fs::read_dir(id.path()).ok()?.flatten() {
                if file.path().to_string_lossy().ends_with(ext) {
                    return Some(file.path());
                }
            }
        }
    }
    None
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
