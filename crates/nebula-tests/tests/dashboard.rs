//! The dashboard engine on a throwaway database: the registry built from
//! declared widgets (dashboards, defaults, duplicate rejection), layout
//! persistence per user per dashboard, and real app widgets loading
//! against a seeded tenant — a stat, a chart and a table each shaping
//! their payload the way their kind promises.
//!
//! Skips when NEBULA_TEST_DATABASE_URL is unset.

use nebula::config::{Config, DatabaseConfig, MigrationsConfig};
use nebula::{
    Dashboards, Kernel, Module, ModuleContext, PlacedWidget, WidgetCx, WidgetDefinition,
    WidgetKind, db,
};
use nebula_apps::accounting::seed as acc_seed;
use nebula_apps::accounting::widgets as acc_widgets;
use nebula_apps::scm::inventory::widgets as inv_widgets;
use nebula_apps::scm::pos::widgets as pos_widgets;
use nebula_apps::scm::seed as scm_seed;
use sea_orm::ConnectionTrait;
use std::sync::Arc;
use uuid::Uuid;

macro_rules! ensure {
    ($cond:expr, $($msg:tt)*) => {
        if !($cond) {
            return Err(format!($($msg)*));
        }
    };
}

/// Declares a representative slice of the real widgets: enough to give
/// the registry two dashboards, defaults in a known order, and payloads
/// of three different kinds.
struct DashboardHarness;

impl Module for DashboardHarness {
    fn name(&self) -> &'static str {
        "dashboard-harness"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.declare_widget(Arc::new(pos_widgets::TakingsTodayWidget));
        ctx.declare_widget(Arc::new(pos_widgets::WeekTrendWidget));
        ctx.declare_widget(Arc::new(pos_widgets::OpenSessionsWidget));
        ctx.declare_widget(Arc::new(inv_widgets::StockValueWidget));
        ctx.declare_widget(Arc::new(acc_widgets::CashPositionWidget));
    }
}

#[test]
fn dashboard_engine() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime must build")
                .block_on(harness());
        })
        .expect("test thread must spawn")
        .join()
        .expect("test thread must not panic");
}

async fn harness() {
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
    let fresh = format!("nebula_dash_{}", Uuid::new_v4().simple());
    admin
        .execute_unprepared(&format!("CREATE DATABASE {fresh}"))
        .await
        .expect("must create the fresh database");

    let outcome = Box::pin(run(&swap_database(&main_url, &fresh))).await;

    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {fresh} WITH (FORCE)"))
        .await;

    outcome.expect("dashboard flow must pass");
}

async fn run(url: &str) -> Result<(), String> {
    let mut config = Config::default();
    config.auth.jwt_secret = "dashboard-test-secret".into();
    config.database = DatabaseConfig {
        url: url.into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.migrations = MigrationsConfig {
        root: format!("{}/../../migrations", env!("CARGO_MANIFEST_DIR")),
    };

    let app = Kernel::builder()
        .with_config(config)
        .add_module(DashboardHarness)
        .build()
        .map_err(|e| format!("kernel must build: {e}"))?
        .init()
        .await
        .map_err(|e| format!("boot must succeed: {e}"))?;
    let db = app.database().ok_or("database must exist")?.clone();

    acc_seed::seed_defaults(&db, "KES")
        .await
        .map_err(|e| format!("accounting seed: {e}"))?;
    scm_seed::seed_defaults(&db, "KES")
        .await
        .map_err(|e| format!("scm seed: {e}"))?;

    // --- The registry -----------------------------------------------------
    let dashboards = app.dashboards();
    ensure!(dashboards.len() == 5, "five widgets registered, not {}", dashboards.len());
    ensure!(
        dashboards.dashboards() == vec!["accounting", "inventory", "pos"],
        "the declared dashboards are the ones widgets named: {:?}",
        dashboards.dashboards()
    );
    ensure!(dashboards.contains_dashboard("pos"), "pos dashboard must exist");
    ensure!(!dashboards.contains_dashboard("sales"), "no sales widgets were declared here");
    ensure!(
        dashboards.get("pos-takings-today").is_some(),
        "widgets are found by name"
    );

    // The default layout follows declared positions (takings=1, week=4,
    // sessions=5) and carries each widget's default span.
    let default = dashboards.default_layout("pos");
    ensure!(
        default.iter().map(|p| p.widget.as_str()).collect::<Vec<_>>()
            == vec!["pos-takings-today", "pos-week-trend", "pos-open-sessions"],
        "default layout must follow declared positions: {default:?}"
    );
    ensure!(
        default[0].span == 3 && default[1].span == 6,
        "default spans come from the widget kind: {default:?}"
    );

    // Duplicate names refuse to build — a boot-time error, not a shadow.
    let dup = Dashboards::build(vec![
        Arc::new(pos_widgets::TakingsTodayWidget),
        Arc::new(pos_widgets::TakingsTodayWidget),
    ]);
    ensure!(dup.is_err(), "duplicate widget names must be rejected");

    // --- Layout persistence and validation --------------------------------
    let user = Uuid::new_v4();
    ensure!(
        dashboards
            .layout(&db, user, "pos")
            .await
            .map_err(|e| format!("layout load: {e}"))?
            .is_none(),
        "a fresh user has no saved layout"
    );
    let picked = vec![
        PlacedWidget { widget: "pos-open-sessions".into(), span: 12 },
        PlacedWidget { widget: "pos-takings-today".into(), span: 4 },
    ];
    dashboards
        .save_layout(&db, user, "pos", &picked)
        .await
        .map_err(|e| format!("layout save: {e}"))?;
    let stored = dashboards
        .layout(&db, user, "pos")
        .await
        .map_err(|e| format!("layout reload: {e}"))?
        .ok_or("saved layout must load back")?;
    ensure!(
        stored.len() == 2 && stored[0].widget == "pos-open-sessions" && stored[0].span == 12,
        "the arrangement survives the round trip: {stored:?}"
    );

    // Saving again replaces, per user per dashboard.
    dashboards
        .save_layout(&db, user, "pos", &picked[..1])
        .await
        .map_err(|e| format!("layout resave: {e}"))?;
    let replaced = dashboards
        .layout(&db, user, "pos")
        .await
        .map_err(|e| format!("layout reload 2: {e}"))?
        .ok_or("replaced layout must load back")?;
    ensure!(replaced.len() == 1, "a save replaces the previous arrangement");
    let other_user = Uuid::new_v4();
    ensure!(
        dashboards
            .layout(&db, other_user, "pos")
            .await
            .map_err(|e| format!("other user load: {e}"))?
            .is_none(),
        "layouts are per user"
    );

    // What save_layout refuses: too many widgets, strangers, foreign
    // widgets, silly spans, duplicates.
    let too_many: Vec<PlacedWidget> = (0..13)
        .map(|i| PlacedWidget { widget: format!("w{i}"), span: 3 })
        .collect();
    ensure!(
        dashboards.save_layout(&db, user, "pos", &too_many).await.is_err(),
        "13 widgets exceed the canvas cap"
    );
    let unknown = [PlacedWidget { widget: "no-such-widget".into(), span: 3 }];
    ensure!(
        dashboards.save_layout(&db, user, "pos", &unknown).await.is_err(),
        "unknown widgets are refused"
    );
    let foreign = [PlacedWidget { widget: "inventory-stock-value".into(), span: 3 }];
    ensure!(
        dashboards.save_layout(&db, user, "pos", &foreign).await.is_err(),
        "a widget of another dashboard is refused"
    );
    let wide = [PlacedWidget { widget: "pos-takings-today".into(), span: 13 }];
    ensure!(
        dashboards.save_layout(&db, user, "pos", &wide).await.is_err(),
        "spans beyond the grid are refused"
    );
    let twice = [
        PlacedWidget { widget: "pos-takings-today".into(), span: 3 },
        PlacedWidget { widget: "pos-takings-today".into(), span: 3 },
    ];
    ensure!(
        dashboards.save_layout(&db, user, "pos", &twice).await.is_err(),
        "a widget placed twice is refused"
    );

    dashboards
        .reset_layout(&db, user, "pos")
        .await
        .map_err(|e| format!("layout reset: {e}"))?;
    ensure!(
        dashboards
            .layout(&db, user, "pos")
            .await
            .map_err(|e| format!("post-reset load: {e}"))?
            .is_none(),
        "reset returns the user to the default layout"
    );

    // --- Widget data ------------------------------------------------------
    let cx = WidgetCx { db: Some(&db), tenant: None, user_id: user };

    let takings = pos_widgets::TakingsTodayWidget
        .load(&cx)
        .await
        .map_err(|e| format!("takings widget: {e}"))?;
    ensure!(takings.kind == WidgetKind::Stat, "takings is a stat");
    let stat = takings.stat.as_ref().ok_or("stat payload must be present")?;
    ensure!(stat.value == "0.00", "an empty day takes nothing: {}", stat.value);

    let trend = pos_widgets::WeekTrendWidget
        .load(&cx)
        .await
        .map_err(|e| format!("trend widget: {e}"))?;
    ensure!(trend.kind == WidgetKind::Chart, "the week trend is a chart");
    let chart = trend.chart.as_ref().ok_or("chart payload must be present")?;
    ensure!(
        chart.labels.len() == 7 && chart.series[0].values.len() == 7,
        "seven day buckets even with no sales"
    );

    let sessions = pos_widgets::OpenSessionsWidget
        .load(&cx)
        .await
        .map_err(|e| format!("sessions widget: {e}"))?;
    ensure!(sessions.kind == WidgetKind::Table, "open sessions is a table");
    let table = sessions.table.as_ref().ok_or("table payload must be present")?;
    ensure!(table.rows.is_empty(), "no sessions are open on a fresh tenant");
    ensure!(table.columns.len() == 4, "the table declares its columns");

    let stock = inv_widgets::StockValueWidget
        .load(&cx)
        .await
        .map_err(|e| format!("stock widget: {e}"))?;
    ensure!(
        stock.stat.as_ref().is_some_and(|s| s.value == "0.00"),
        "a fresh tenant holds no stock value"
    );

    let cash = acc_widgets::CashPositionWidget
        .load(&cx)
        .await
        .map_err(|e| format!("cash widget: {e}"))?;
    ensure!(
        cash.stat.as_ref().is_some_and(|s| s.value == "0.00"),
        "a fresh tenant's books hold no cash"
    );

    Ok(())
}

/// Swap the database name in a postgres URL for the throwaway one.
fn swap_database(url: &str, database: &str) -> String {
    let base = url.rsplit_once('/').map(|(b, _)| b).unwrap_or(url);
    format!("{base}/{database}")
}
