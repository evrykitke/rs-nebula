//! The POS submodule end to end on a throwaway database: registers,
//! session discipline (open race, drawer events, X figures), sale
//! capture (idempotency, tender rules, batch demands, price drift,
//! voids, refunds), the catalog feed, and the heart of the design —
//! session close consolidating a whole day into one stock movement and
//! one revenue GL entry, with the ledger and reconciliation tying out to
//! exact decimals. The GL port runs in process (accounting books what
//! POS publishes), so every assertion is synchronous.
//!
//! Skips when NEBULA_TEST_DATABASE_URL is unset.

use nebula::config::{Config, DatabaseConfig, MigrationsConfig};
use nebula::{Kernel, Module, ModuleContext, Reset, SeriesDef, db};
use nebula_apps::accounting::gl_port::GlPort;
use nebula_apps::accounting::journal::{entry, posting};
use nebula_apps::accounting::{account, fiscal, seed as acc_seed, tax};
use nebula_apps::scm::gl::{Gl, outbox, reconciliation, subscribe_acks};
use nebula_apps::scm::inventory::batch::batch;
use nebula_apps::scm::inventory::item::{self, CostingMethod, ItemBody, ItemType};
use nebula_apps::scm::inventory::moves::{LineInput, MoveService, MoveType, NewMove, doc as move_doc};
use nebula_apps::scm::inventory::stock;
use nebula_apps::scm::inventory::warehouse;
use nebula_apps::scm::pos::register::{RegisterBody, Store as RegisterStore};
use nebula_apps::scm::pos::sale::{
    self, NewRefund, NewSale, OrderKind, OrderStatus, RefundLineInput, SaleLineInput,
    SaleService, TenderInput,
};
use nebula_apps::scm::pos::reports::PosQueries;
use nebula_apps::scm::pos::session::{
    CountInput, DenominationCount, SessionService, SessionStatus,
};
use nebula_apps::scm::pos::settings::{self as pos_settings, Settings};
use nebula_apps::scm::seed as scm_seed;
use rust_decimal::Decimal;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
};
use uuid::Uuid;

/// Fails the run with a formatted message (keeps the throwaway-database
/// cleanup on the happy path — a panic would skip the `DROP`).
macro_rules! ensure {
    ($cond:expr, $($msg:tt)*) => {
        if !($cond) {
            return Err(format!($($msg)*));
        }
    };
}

/// Both ends of the GL port on one bus, plus every series POS pulls
/// from, without the full apps' background workers.
struct PosHarness;

impl Module for PosHarness {
    fn name(&self) -> &'static str {
        "pos-harness"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        for (key, name, template) in [
            ("inventory.receipt", "Goods Receipt", "GRN-{YYYY}-{SEQ:5}"),
            ("inventory.issue", "Stock Issue", "ISS-{YYYY}-{SEQ:5}"),
            ("pos.receipt", "POS Receipt", "RCP-{YYYY}-{SEQ:6}"),
            ("pos.session", "POS Session", "PS-{YYYY}-{SEQ:5}"),
            (
                "accounting.system",
                "System Journal Entry",
                "SYS-{YYYY}-{SEQ:5}",
            ),
        ] {
            ctx.declare_series(
                SeriesDef::new(key, name, template, Reset::Yearly).expect("valid series template"),
            );
        }
        GlPort::subscribe(ctx);
        subscribe_acks(ctx);
    }
}

#[test]
fn pos_end_to_end() {
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
    let fresh = format!("nebula_pos_{}", Uuid::new_v4().simple());
    admin
        .execute_unprepared(&format!("CREATE DATABASE {fresh}"))
        .await
        .expect("must create the fresh database");

    let outcome = Box::pin(run(&swap_database(&main_url, &fresh))).await;

    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {fresh} WITH (FORCE)"))
        .await;

    outcome.expect("POS flow must pass");
}

async fn run(url: &str) -> Result<(), String> {
    let mut config = Config::default();
    config.auth.jwt_secret = "pos-test-secret".into();
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
        .add_module(PosHarness)
        .build()
        .map_err(|e| format!("kernel must build: {e}"))?
        .init()
        .await
        .map_err(|e| format!("boot must succeed: {e}"))?;
    let db = app.database().ok_or("database must exist")?.clone();

    acc_seed::seed_defaults(&db, "KES")
        .await
        .map_err(|e| format!("accounting seed: {e}"))?;
    fiscal::FiscalService::new(db.clone())
        .ensure_current_year()
        .await
        .map_err(|e| format!("fiscal year seed: {e}"))?;
    scm_seed::seed_defaults(&db, "KES")
        .await
        .map_err(|e| format!("scm seed: {e}"))?;

    // The three POS clearing roles arrived with the accounting seed.
    for role in ["mpesa_clearing", "card_clearing", "cash_over_short"] {
        let hit = account::Entity::find()
            .filter(account::Column::SystemKey.eq(role))
            .count(&db)
            .await
            .map_err(|e| format!("role lookup: {e}"))?;
        ensure!(hit == 1, "seeded account for role {role:?} must exist");
    }

    let numbering = app.numbering();
    let events = app.events();
    let gl = Gl::new(events.clone(), None);

    let items = item::Store::new(db.clone());
    let unit = items
        .list_uoms()
        .await
        .map_err(|e| format!("list uoms: {e}"))?
        .into_iter()
        .find(|u| u.code == "unit")
        .ok_or("seeded 'unit' uom")?;
    let main = warehouse::Store::new(db.clone())
        .find_all()
        .await
        .map_err(|e| format!("warehouses: {e}"))?
        .into_iter()
        .find(|w| w.code == "MAIN")
        .ok_or("seeded MAIN warehouse")?;
    let vat16 = tax::Entity::find()
        .filter(tax::Column::Code.eq("VAT16"))
        .one(&db)
        .await
        .map_err(|e| format!("vat16 lookup: {e}"))?
        .ok_or("seeded VAT16 tax code")?;

    // ---- catalog of the shop -------------------------------------------
    // Shelf prices are VAT-inclusive; WATER carries 16% inside its 116.
    let mut water = item_body("WTR-1", "Bottled Water", unit.id);
    water.selling_price = Some(dec(116));
    water.sales_tax_code_id = Some(vat16.id);
    let water = items
        .create_item(water, None)
        .await
        .map_err(|e| format!("water: {e}"))?;

    let mut bread = item_body("BRD-1", "Bread", unit.id);
    bread.selling_price = Some(dec(50));
    let bread = items
        .create_item(bread, None)
        .await
        .map_err(|e| format!("bread: {e}"))?;

    let mut pill = item_body("PIL-1", "Painkiller", unit.id);
    pill.selling_price = Some(dec(20));
    pill.track_batches = true;
    let pill = items
        .create_item(pill, None)
        .await
        .map_err(|e| format!("pill: {e}"))?;

    let mut phone = item_body("PHN-1", "Phone", unit.id);
    phone.selling_price = Some(dec(9999));
    phone.track_serials = true;
    let phone = items
        .create_item(phone, None)
        .await
        .map_err(|e| format!("phone: {e}"))?;

    let mut low = item_body("LOW-1", "Scarce Thing", unit.id);
    low.selling_price = Some(dec(10));
    let low = items
        .create_item(low, None)
        .await
        .map_err(|e| format!("low: {e}"))?;

    // Opening stock: WATER 100 @ 60, BREAD 50 @ 20, PILL 30 @ 10 in lot
    // B1, LOW 2 @ 5.
    let moves = MoveService::new(db.clone());
    let opening = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![
                move_line(water.id, dec(100), Some(dec(60)), None),
                move_line(bread.id, dec(50), Some(dec(20)), None),
                move_line(pill.id, dec(30), Some(dec(10)), Some("B1")),
                move_line(low.id, dec(2), Some(dec(5)), None),
            ],
        ))
        .await
        .map_err(|e| format!("opening draft: {e}"))?;
    moves
        .post(opening.id, &numbering, &gl)
        .await
        .map_err(|e| format!("opening post: {e}"))?;
    let b1 = batch::Entity::find()
        .filter(batch::Column::ItemId.eq(pill.id))
        .filter(batch::Column::BatchNo.eq("B1"))
        .one(&db)
        .await
        .map_err(|e| format!("b1 lookup: {e}"))?
        .ok_or("lot B1 must exist after the receipt")?;

    // ---- registers ------------------------------------------------------
    let registers = RegisterStore::new(db.clone());
    let till1 = registers
        .create(register_body("TILL-1", "Front Till", main.id), None)
        .await
        .map_err(|e| format!("till1: {e}"))?;
    let till2 = registers
        .create(register_body("TILL-2", "Back Till", main.id), None)
        .await
        .map_err(|e| format!("till2: {e}"))?;
    ensure!(
        registers
            .create(register_body("TILL-1", "Copycat", main.id), None)
            .await
            .is_err(),
        "duplicate register code must be rejected"
    );
    let gridded = registers
        .set_grid(till1.id, serde_json::json!({ "tiles": ["WTR-1", "BRD-1"] }))
        .await
        .map_err(|e| format!("grid: {e}"))?;
    ensure!(gridded.grid_layout.is_some(), "the grid layout must persist");

    // ---- session discipline --------------------------------------------
    let sessions = SessionService::new(db.clone());
    let cashier = Uuid::new_v4();
    ensure!(
        sessions
            .open(till1.id, dec(-5), cashier, &numbering)
            .await
            .is_err(),
        "a negative float must be rejected"
    );
    let s1 = sessions
        .open(till1.id, dec(1000), cashier, &numbering)
        .await
        .map_err(|e| format!("open s1: {e}"))?;
    ensure!(
        s1.number.as_deref().is_some_and(|n| n.starts_with("PS-")),
        "sessions number from the PS series, got {:?}",
        s1.number
    );
    let dup = sessions.open(till1.id, dec(0), cashier, &numbering).await;
    ensure!(
        dup.is_err(),
        "a second open on the same register must be refused"
    );
    ensure!(
        sessions
            .current(till1.id)
            .await
            .map_err(|e| format!("current: {e}"))?
            .is_some_and(|v| v.id == s1.id),
        "current must resume the open session"
    );

    // The open race: two cashiers grab the same till together — the
    // partial unique index lets exactly one through.
    let (a, b) = tokio::join!(
        open_race(db.clone(), numbering.clone(), till2.id),
        open_race(db.clone(), numbering.clone(), till2.id),
    );
    ensure!(
        a.is_ok() != b.is_ok(),
        "exactly one concurrent open must win (got {a:?} / {b:?})"
    );
    // Free till2 for the overshoot scenario later: close the winner's
    // empty session (nothing sold, float zero — no counts needed).
    let till2_session = a.or(b).unwrap();
    sessions
        .close(till2_session, vec![], None, 0, None, &gl)
        .await
        .map_err(|e| format!("close empty till2 session: {e}"))?;

    // ---- selling --------------------------------------------------------
    let sales = SaleService::new(db.clone());

    // S1: 3 water, cash 500 handed over → 152 change; VAT inside = 48.
    // The till also reports its tempo: 30 seconds, 5 inputs.
    let s1_sale = {
        let mut ns = simple_sale(s1.id, vec![line(water.id, 3, 116, None)], vec![cash(348, Some(500))]);
        ns.capture_seconds = Some(30);
        ns.input_count = Some(5);
        sales.capture(ns, &numbering).await.map_err(|e| format!("S1: {e}"))?
    };
    ensure!(
        s1_sale.number.as_deref().is_some_and(|n| n.starts_with("RCP-")),
        "sales number from the RCP series"
    );
    ensure!(s1_sale.total == dec(348), "S1 total, got {}", s1_sale.total);
    ensure!(s1_sale.tax_total == dec(48), "S1 VAT inside, got {}", s1_sale.tax_total);
    ensure!(s1_sale.change == dec(152), "S1 change, got {}", s1_sale.change);

    // Idempotency: replaying S1's client UUID returns the same order.
    let replay = sales
        .capture(
            simple_sale_with_uuid(
                s1_sale.client_uuid,
                s1.id,
                vec![line(water.id, 3, 116, None)],
                vec![cash(348, Some(500))],
            ),
            &numbering,
        )
        .await
        .map_err(|e| format!("S1 replay: {e}"))?;
    ensure!(replay.id == s1_sale.id, "a replay must return the same order");

    // S2: 2 bread by M-Pesa (code required).
    ensure!(
        sales
            .capture(
                simple_sale(
                    s1.id,
                    vec![line(bread.id, 2, 50, None)],
                    vec![TenderInput {
                        tender: "mpesa".into(),
                        amount: dec(100),
                        tendered: None,
                        reference: None,
                    }],
                ),
                &numbering,
            )
            .await
            .is_err(),
        "an M-Pesa tender without its code must be refused"
    );
    {
        let mut ns = simple_sale(
            s1.id,
            vec![line(bread.id, 2, 50, None)],
            vec![TenderInput {
                tender: "mpesa".into(),
                amount: dec(100),
                tendered: None,
                reference: Some("SFB0X1TEST".into()),
            }],
        );
        // Tempo: 50 seconds, 7 inputs — with S1 that averages 40 / 6.
        ns.capture_seconds = Some(50);
        ns.input_count = Some(7);
        sales.capture(ns, &numbering).await.map_err(|e| format!("S2: {e}"))?;
    }

    // S3: 1 pill from lot B1 by card; the lot is mandatory.
    ensure!(
        sales
            .capture(
                simple_sale(s1.id, vec![line(pill.id, 1, 20, None)], vec![card(20)]),
                &numbering,
            )
            .await
            .is_err(),
        "a batch-tracked item without its lot must be refused"
    );
    sales
        .capture(
            simple_sale(s1.id, vec![line(pill.id, 1, 20, Some(b1.id))], vec![card(20)]),
            &numbering,
        )
        .await
        .map_err(|e| format!("S3: {e}"))?;

    // The guard rails around capture.
    ensure!(
        sales
            .capture(
                simple_sale(s1.id, vec![line(phone.id, 1, 9999, None)], vec![cash(9999, None)]),
                &numbering,
            )
            .await
            .is_err(),
        "a serial-tracked item cannot be sold at the till"
    );
    ensure!(
        sales
            .capture(
                simple_sale(s1.id, vec![line(bread.id, 1, 50, None)], vec![cash(45, None)]),
                &numbering,
            )
            .await
            .is_err(),
        "tenders that do not sum to the total must be refused"
    );
    ensure!(
        sales
            .capture(
                simple_sale(s1.id, vec![line(bread.id, 1, 45, None)], vec![cash(45, None)]),
                &numbering,
            )
            .await
            .is_err(),
        "an online capture at a stale price must be refused"
    );
    let mut manual = simple_sale(s1.id, vec![], vec![cash(70, None)]);
    manual.lines = vec![SaleLineInput {
        item_id: bread.id,
        qty: dec(1),
        unit_price: dec(70),
        manual_price: true,
        discount_pct: None,
        batch_id: None,
    }];
    ensure!(
        sales.capture(manual, &numbering).await.is_err(),
        "a manual price without the override permission must be refused"
    );

    // S5: 2 bread at 10% off — override allowed → 90 cash.
    let mut s5 = simple_sale(s1.id, vec![], vec![cash(90, None)]);
    s5.allow_override = true;
    s5.lines = vec![SaleLineInput {
        item_id: bread.id,
        qty: dec(2),
        unit_price: dec(50),
        manual_price: false,
        discount_pct: Some(dec(10)),
        batch_id: None,
    }];
    let s5_sale = sales
        .capture(s5, &numbering)
        .await
        .map_err(|e| format!("S5: {e}"))?;
    ensure!(s5_sale.total == dec(90), "S5 total, got {}", s5_sale.total);
    ensure!(
        s5_sale.discount_total == dec(10),
        "S5 discount, got {}",
        s5_sale.discount_total
    );

    // S6: an offline sale that cached yesterday's bread price — the
    // client's 45 stands and the drift is flagged.
    let mut s6 = simple_sale(s1.id, vec![line(bread.id, 1, 45, None)], vec![cash(45, None)]);
    s6.captured_offline = true;
    let s6_sale = sales
        .capture(s6, &numbering)
        .await
        .map_err(|e| format!("S6: {e}"))?;
    ensure!(s6_sale.price_drift, "the offline price drift must be flagged");
    ensure!(s6_sale.total == dec(45), "S6 keeps the client price");

    // SV: a mis-scan, voided away (PIN gating lives at the HTTP layer;
    // the service enforces the lifecycle rules).
    let sv = sales
        .capture(
            simple_sale(s1.id, vec![line(bread.id, 1, 50, None)], vec![cash(50, None)]),
            &numbering,
        )
        .await
        .map_err(|e| format!("SV: {e}"))?;
    let sv = sales
        .void(sv.id, "mis-scan", None)
        .await
        .map_err(|e| format!("void SV: {e}"))?;
    ensure!(sv.status == OrderStatus::Voided, "SV must be voided");
    ensure!(
        sales.void(sv.id, "again", None).await.is_err(),
        "a voided order cannot void twice"
    );

    // R1: one water back, cash out of the drawer at the original money.
    let water_line = s1_sale
        .lines
        .iter()
        .find(|l| l.item_id == water.id)
        .ok_or("S1 water line")?;
    let r1 = sales
        .refund(
            NewRefund {
                client_uuid: Uuid::new_v4(),
                session_id: s1.id,
                original_id: s1_sale.id,
                lines: vec![RefundLineInput {
                    line_id: water_line.id,
                    qty: dec(1),
                }],
                tender: "cash".into(),
                reference: None,
                created_by: None,
            },
            &numbering,
        )
        .await
        .map_err(|e| format!("R1: {e}"))?;
    ensure!(r1.kind == OrderKind::Refund, "R1 is a refund");
    ensure!(r1.total == dec(116), "R1 refunds one water, got {}", r1.total);
    ensure!(r1.tax_total == dec(16), "R1 refunds its VAT, got {}", r1.tax_total);
    ensure!(
        sales
            .refund(
                NewRefund {
                    client_uuid: Uuid::new_v4(),
                    session_id: s1.id,
                    original_id: s1_sale.id,
                    lines: vec![RefundLineInput {
                        line_id: water_line.id,
                        qty: dec(3),
                    }],
                    tender: "cash".into(),
                    reference: None,
                    created_by: None,
                },
                &numbering,
            )
            .await
            .is_err(),
        "a refund beyond the un-refunded remainder must be refused"
    );
    ensure!(
        sales.void(s1_sale.id, "too late", None).await.is_err(),
        "an order with refunds against it cannot be voided"
    );

    // Drawer events: 200 in (float top-up), 300 out (courier).
    sessions
        .cash_movement(s1.id, "paid_in", dec(200), "float top-up", None)
        .await
        .map_err(|e| format!("paid in: {e}"))?;
    sessions
        .cash_movement(s1.id, "paid_out", dec(300), "courier", None)
        .await
        .map_err(|e| format!("paid out: {e}"))?;
    ensure!(
        sessions
            .cash_movement(s1.id, "paid_out", dec(0), "nothing", None)
            .await
            .is_err(),
        "a zero drawer movement must be refused"
    );

    // ---- the X report ---------------------------------------------------
    // Cash: 348 + 90 + 45 sold, 116 refunded → 367 net; expected drawer
    // 1000 float + 367 + 200 in − 300 out = 1267.
    let x = sessions
        .x_report(s1.id, false)
        .await
        .map_err(|e| format!("x report: {e}"))?;
    ensure!(x.orders == 5, "five captured sales, got {}", x.orders);
    ensure!(x.refunds == 1 && x.voids == 1, "one refund, one void");
    ensure!(x.price_drift == 1 && x.offline == 1, "one drifted offline sale");
    ensure!(x.gross_sales == dec(603), "gross sales, got {}", x.gross_sales);
    ensure!(x.refund_total == dec(116), "refund total, got {}", x.refund_total);
    ensure!(x.tax_total == dec(32), "net VAT, got {}", x.tax_total);
    ensure!(
        x.expected_cash == Some(dec(1267)),
        "expected cash, got {:?}",
        x.expected_cash
    );
    let x_cash = x.tenders.iter().find(|t| t.tender == "cash").ok_or("cash line")?;
    ensure!(x_cash.net == dec(367), "cash net, got {}", x_cash.net);
    let x_mpesa = x.tenders.iter().find(|t| t.tender == "mpesa").ok_or("mpesa line")?;
    ensure!(x_mpesa.net == dec(100), "mpesa net, got {}", x_mpesa.net);
    ensure!(
        x.avg_sale_seconds == Some(dec(40)) && x.avg_sale_inputs == Some(dec(6)),
        "live tempo averages over the sales that reported, got {:?}/{:?}",
        x.avg_sale_seconds,
        x.avg_sale_inputs
    );

    // A blind X withholds the cash expectation and the cash line — the
    // count proceeds with no number to count to; the record tenders stay.
    let blind = sessions
        .x_report(s1.id, true)
        .await
        .map_err(|e| format!("blind x report: {e}"))?;
    ensure!(
        blind.blind && blind.expected_cash.is_none(),
        "a blind X must withhold the cash expectation"
    );
    ensure!(
        blind.tenders.iter().all(|t| t.tender != "cash")
            && blind.tenders.iter().any(|t| t.tender == "mpesa"),
        "a blind X drops the cash line and keeps the record tenders"
    );

    // ---- closing --------------------------------------------------------
    // The cash count arrives with its count sheet: 1×1000 + 1×200 + 2×20.
    let full_counts = || {
        vec![
            CountInput {
                tender: "cash".into(),
                counted: dec(1240),
                denominations: Some(vec![
                    DenominationCount { denom: dec(1000), count: 1 },
                    DenominationCount { denom: dec(200), count: 1 },
                    DenominationCount { denom: dec(20), count: 2 },
                ]),
            },
            CountInput {
                tender: "mpesa".into(),
                counted: dec(100),
                denominations: None,
            },
            CountInput {
                tender: "card".into(),
                counted: dec(20),
                denominations: None,
            },
        ]
    };
    // A count sheet that does not add up to its count is refused.
    ensure!(
        sessions
            .close(
                s1.id,
                vec![
                    CountInput {
                        tender: "cash".into(),
                        counted: dec(1240),
                        denominations: Some(vec![DenominationCount {
                            denom: dec(1000),
                            count: 1,
                        }]),
                    },
                    CountInput {
                        tender: "mpesa".into(),
                        counted: dec(100),
                        denominations: None,
                    },
                    CountInput {
                        tender: "card".into(),
                        counted: dec(20),
                        denominations: None,
                    },
                ],
                Some("x".into()),
                0,
                None,
                &gl,
            )
            .await
            .is_err(),
        "a count sheet that does not sum to the count must be refused"
    );
    ensure!(
        sessions
            .close(s1.id, full_counts(), Some("x".into()), 3, None, &gl)
            .await
            .is_err(),
        "a close with unsynced sales must be refused"
    );
    ensure!(
        sessions
            .close(
                s1.id,
                vec![CountInput {
                    tender: "cash".into(),
                    counted: dec(1240),
                    denominations: None,
                }],
                Some("x".into()),
                0,
                None,
                &gl,
            )
            .await
            .is_err(),
        "a tender with takings must be counted"
    );
    ensure!(
        sessions
            .close(s1.id, full_counts(), None, 0, None, &gl)
            .await
            .is_err(),
        "a shortage without a note must be refused"
    );
    let closed = sessions
        .close(
            s1.id,
            full_counts(),
            Some("27 short - see incident book".into()),
            0,
            None,
            &gl,
        )
        .await
        .map_err(|e| format!("close s1: {e}"))?;
    ensure!(closed.status == SessionStatus::Closed, "s1 must close");
    let move_id = closed.move_id.ok_or("the close must record its movement")?;
    ensure!(
        sessions
            .close(s1.id, full_counts(), None, 0, None, &gl)
            .await
            .is_err(),
        "a closed session cannot close twice"
    );
    ensure!(
        sales
            .capture(
                simple_sale(s1.id, vec![line(bread.id, 1, 50, None)], vec![cash(50, None)]),
                &numbering,
            )
            .await
            .is_err(),
        "no sale exists outside an open session"
    );

    // One movement, stamped with the session's own number, net of the
    // refund and the void: water 2 out, bread 5 out, pill 1 out of B1.
    let mv = move_doc::Entity::find_by_id(move_id)
        .one(&db)
        .await
        .map_err(|e| format!("movement: {e}"))?
        .ok_or("consolidated movement must exist")?;
    ensure!(
        mv.number == closed.number,
        "the movement carries the session number: {:?} vs {:?}",
        mv.number,
        closed.number
    );
    ensure!(
        mv.source.as_deref() == Some(&format!("pos.session:{}", s1.id)),
        "the movement is source-stamped, got {:?}",
        mv.source
    );
    ensure!(
        level_of(&db, water.id, main.id).await == (dec(98), dec(5880)),
        "water level after close"
    );
    ensure!(
        level_of(&db, bread.id, main.id).await == (dec(45), dec(900)),
        "bread level after close"
    );
    ensure!(
        level_of(&db, pill.id, main.id).await == (dec(29), dec(290)),
        "pill level after close"
    );
    let b1_left = stock::ledger::Entity::find()
        .filter(stock::ledger::Column::BatchId.eq(b1.id))
        .all(&db)
        .await
        .map_err(|e| format!("b1 ledger: {e}"))?
        .iter()
        .map(|r| r.qty_delta)
        .sum::<Decimal>();
    ensure!(b1_left == dec(29), "lot B1 holds 29 after the session, got {b1_left}");

    // COGS rode on the movement: 2×60 + 5×20 + 1×10 = 230.
    let cogs_source = format!("pos.session:{}:cogs", s1.id);
    let ps = booked_entry(&db, &cogs_source).await?;
    assert_leg(&db, &ps, "cogs", dec(230), Decimal::ZERO, &cogs_source).await?;
    assert_leg(&db, &ps, "inventory", Decimal::ZERO, dec(230), &cogs_source).await?;

    // The revenue entry: Dr cash at the counted effect (367 − 27), the
    // clearings at their takings, the shortage as an expense, revenue
    // and VAT on the credit side — balancing exactly.
    let close_source = format!("pos.session:{}:close", s1.id);
    let ps = booked_entry(&db, &close_source).await?;
    assert_leg(&db, &ps, "cash", dec(340), Decimal::ZERO, &close_source).await?;
    assert_leg(&db, &ps, "mpesa_clearing", dec(100), Decimal::ZERO, &close_source).await?;
    assert_leg(&db, &ps, "card_clearing", dec(20), Decimal::ZERO, &close_source).await?;
    assert_leg(&db, &ps, "cash_over_short", dec(27), Decimal::ZERO, &close_source).await?;
    assert_leg(&db, &ps, "sales", Decimal::ZERO, dec(455), &close_source).await?;
    assert_leg(&db, &ps, "vat_output", Decimal::ZERO, dec(32), &close_source).await?;

    // The Z report reads the stored counts forever.
    let z = sessions
        .z_report(s1.id)
        .await
        .map_err(|e| format!("z report: {e}"))?;
    let z_cash = z.tenders.iter().find(|t| t.tender == "cash").ok_or("z cash")?;
    ensure!(
        z_cash.counted == Some(dec(1240)) && z_cash.variance == Some(dec(-27)),
        "the Z report carries the stored count and variance"
    );
    ensure!(
        z.session.closing_note.as_deref() == Some("27 short - see incident book"),
        "the closing note survives"
    );

    // ---- instrumentation settled onto the session record ----------------
    ensure!(
        z.session.avg_sale_seconds == Some(dec(40))
            && z.session.avg_sale_inputs == Some(dec(6))
            && z.session.void_count == Some(1),
        "the close settles the till tempo onto the session, got {:?}/{:?}/{:?}",
        z.session.avg_sale_seconds,
        z.session.avg_sale_inputs,
        z.session.void_count
    );

    // ---- the report queries (what the framework reports print) ----------
    let queries = PosQueries::new(db.clone());

    // The printable Z's data: the stored count sheet and the item summary.
    let zv = queries.z(s1.id).await.map_err(|e| format!("z view: {e}"))?;
    ensure!(
        zv.sheets.len() == 1 && zv.sheets[0].tender == "cash" && zv.sheets[0].lines.len() == 3,
        "the cash count sheet rides on the Z"
    );
    let sheet_total: Decimal = zv.sheets[0]
        .lines
        .iter()
        .map(|l| l.denom * Decimal::from(l.count))
        .sum();
    ensure!(sheet_total == dec(1240), "the stored sheet sums to the count");
    ensure!(
        zv.items
            .iter()
            .any(|i| i.qty == dec(2) && i.gross == dec(232)),
        "the Z item summary nets the refunded water: 3 sold − 1 back = 2 for 232"
    );

    // Session summary over an open window: s1's day, to the shilling.
    let summary = queries
        .sessions(None, None, None)
        .await
        .map_err(|e| format!("sessions summary: {e}"))?;
    let s1_row = summary
        .rows
        .iter()
        .find(|r| r.session_id == s1.id)
        .ok_or("s1 in the summary")?;
    ensure!(
        s1_row.orders == 5
            && s1_row.refunds == 1
            && s1_row.voids == 1
            && s1_row.gross_sales == dec(603)
            && s1_row.net_total == dec(487)
            && s1_row.tax_total == dec(32)
            && s1_row.cash_variance == Some(dec(-27))
            && s1_row.avg_sale_seconds == Some(dec(40)),
        "the session summary row ties out"
    );

    // Tender mix: how the day's money arrived.
    let mix = queries
        .tender_mix(None, None)
        .await
        .map_err(|e| format!("tender mix: {e}"))?;
    ensure!(mix.net_total == dec(487), "mix net, got {}", mix.net_total);
    let mix_of = |t: &str| {
        mix.rows
            .iter()
            .find(|r| r.tender == t)
            .map(|r| r.net)
            .unwrap_or_default()
    };
    ensure!(
        mix_of("cash") == dec(367) && mix_of("mpesa") == dec(100) && mix_of("card") == dec(20),
        "the tender mix nets per tender"
    );

    // Item sales: water sold 3, one came back.
    let items_view = queries
        .item_sales(None, None)
        .await
        .map_err(|e| format!("item sales: {e}"))?;
    let water_row = items_view
        .rows
        .iter()
        .find(|r| r.sku == "WTR-1")
        .ok_or("water in item sales")?;
    ensure!(
        water_row.qty_sold == dec(3)
            && water_row.qty_refunded == dec(1)
            && water_row.gross == dec(232)
            && water_row.tax == dec(32),
        "the water item row ties out"
    );

    // Hourly: whatever hour the test ran in, the sums must tie out.
    let hourly = queries
        .hourly(None, None, 0)
        .await
        .map_err(|e| format!("hourly: {e}"))?;
    let hourly_sales: i64 = hourly.rows.iter().map(|r| r.sales).sum();
    let hourly_net: Decimal = hourly.rows.iter().map(|r| r.net_total).sum();
    ensure!(
        hourly_sales == 5 && hourly_net == dec(487),
        "the hourly buckets sum to the day"
    );

    // ---- POS settings ---------------------------------------------------
    let defaults = pos_settings::load(&db)
        .await
        .map_err(|e| format!("settings defaults: {e}"))?;
    ensure!(
        !defaults.blind_count && defaults.denominations.first() == Some(&dec(1000)),
        "settings default to declared counts and the KES note set"
    );
    pos_settings::save(
        &db,
        &Settings {
            blind_count: true,
            denominations: vec![dec(1000), dec(500), dec(100)],
        },
        None,
    )
    .await
    .map_err(|e| format!("settings save: {e}"))?;
    let stored = pos_settings::load(&db)
        .await
        .map_err(|e| format!("settings reload: {e}"))?;
    ensure!(
        stored.blind_count && stored.denominations == vec![dec(1000), dec(500), dec(100)],
        "saved settings read back"
    );

    // ---- the catalog feed ----------------------------------------------
    let cat = sale::catalog(&db, till1.id, None)
        .await
        .map_err(|e| format!("catalog: {e}"))?;
    ensure!(cat.currency == "KES", "catalog in the walk-in currency");
    let cat_water = cat.items.iter().find(|i| i.sku == "WTR-1").ok_or("water in catalog")?;
    ensure!(cat_water.price == dec(116), "water priced, got {}", cat_water.price);
    ensure!(cat_water.tax_rate == dec(16), "water carries its VAT rate");
    ensure!(cat_water.on_hand == dec(98), "water on-hand, got {}", cat_water.on_hand);
    let cat_pill = cat.items.iter().find(|i| i.sku == "PIL-1").ok_or("pill in catalog")?;
    ensure!(
        cat_pill.batches.len() == 1 && cat_pill.batches[0].on_hand == dec(29),
        "the pill lot rides in the catalog"
    );
    ensure!(
        !cat.items.iter().any(|i| i.sku == "PHN-1"),
        "serial-tracked items stay out of the catalog"
    );
    // Delta: nothing changed since the fetch — then bread does.
    let delta = sale::catalog(&db, till1.id, Some(cat.generated_at))
        .await
        .map_err(|e| format!("empty delta: {e}"))?;
    ensure!(delta.items.is_empty(), "an idle delta is empty, got {}", delta.items.len());
    let mut renamed = item_body("BRD-1", "Sourdough", unit.id);
    renamed.selling_price = Some(dec(50));
    items
        .update_item(bread.id, renamed, None)
        .await
        .map_err(|e| format!("rename bread: {e}"))?;
    let delta = sale::catalog(&db, till1.id, Some(cat.generated_at))
        .await
        .map_err(|e| format!("delta: {e}"))?;
    ensure!(
        delta.items.len() == 1 && delta.items[0].sku == "BRD-1",
        "the delta carries exactly the touched item"
    );

    // ---- the closing-state retry ---------------------------------------
    // A session that sold more than the shelf holds (offline overshoot):
    // counting succeeds, consolidation fails, the session parks in
    // `closing` (sales blocked), and the close retries clean once stock
    // is corrected.
    let s2 = sessions
        .open(till2.id, dec(0), cashier, &numbering)
        .await
        .map_err(|e| format!("open s2: {e}"))?;
    let mut overshoot = simple_sale(s2.id, vec![line(low.id, 5, 10, None)], vec![cash(50, None)]);
    overshoot.captured_offline = true;
    sales
        .capture(overshoot, &numbering)
        .await
        .map_err(|e| format!("overshoot sale: {e}"))?;
    let low_counts = || {
        vec![CountInput {
            tender: "cash".into(),
            counted: dec(50),
            denominations: None,
        }]
    };
    ensure!(
        sessions
            .close(s2.id, low_counts(), None, 0, None, &gl)
            .await
            .is_err(),
        "consolidating 5 of a 2-on-hand item must fail"
    );
    let stuck = sessions.view(s2.id).await.map_err(|e| format!("s2 view: {e}"))?;
    ensure!(
        stuck.status == SessionStatus::Closing,
        "the failed close parks the session closing, got {:?}",
        stuck.status
    );
    ensure!(
        sales
            .capture(
                simple_sale(s2.id, vec![line(low.id, 1, 10, None)], vec![cash(10, None)]),
                &numbering,
            )
            .await
            .is_err(),
        "a closing session no longer sells"
    );
    // The shelf is corrected (a found case in the back room)…
    let topup = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(low.id, dec(10), Some(dec(5)), None)],
        ))
        .await
        .map_err(|e| format!("topup draft: {e}"))?;
    moves
        .post(topup.id, &numbering, &gl)
        .await
        .map_err(|e| format!("topup post: {e}"))?;
    // …and the very same close call now completes on the stored counts.
    let closed2 = sessions
        .close(s2.id, low_counts(), None, 0, None, &gl)
        .await
        .map_err(|e| format!("retry close s2: {e}"))?;
    ensure!(closed2.status == SessionStatus::Closed, "the retry closes s2");
    ensure!(
        level_of(&db, low.id, main.id).await == (dec(7), dec(35)),
        "the scarce item's level after the retried close"
    );

    // ---- global invariants ---------------------------------------------
    let all = posting::Entity::find()
        .all(&db)
        .await
        .map_err(|e| format!("all postings: {e}"))?;
    let debits: Decimal = all.iter().map(|p| p.debit).sum();
    let credits: Decimal = all.iter().map(|p| p.credit).sum();
    ensure!(
        debits == credits,
        "double entry must hold: debits {debits} != credits {credits}"
    );

    let staged = outbox::Entity::find()
        .count(&db)
        .await
        .map_err(|e| format!("outbox count: {e}"))?;
    ensure!(staged == 0, "{staged} rows linger in the outbox");

    let recon = reconciliation(&db)
        .await
        .map_err(|e| format!("reconciliation: {e}"))?;
    ensure!(
        recon.inventory_gap == Some(Decimal::ZERO),
        "the stock/GL reconciliation must read zero, got {:?}",
        recon.inventory_gap
    );

    Ok(())
}

/// One contender in the concurrent-open race; returns the session id.
async fn open_race(
    db: DatabaseConnection,
    numbering: nebula::Numbering,
    register_id: Uuid,
) -> Result<Uuid, String> {
    let handle = tokio::spawn(async move {
        SessionService::new(db)
            .open(register_id, Decimal::ZERO, Uuid::new_v4(), &numbering)
            .await
            .map(|v| v.id)
            .map_err(|e| e.to_string())
    });
    handle.await.map_err(|e| format!("join: {e}"))?
}

// ---------------------------------------------------------------------------
// Assertion helpers (as the scm_gl suite)
// ---------------------------------------------------------------------------

async fn booked_entry(
    db: &DatabaseConnection,
    source: &str,
) -> Result<Vec<posting::Model>, String> {
    let row = entry::Entity::find()
        .filter(entry::Column::Reference.eq(source))
        .one(db)
        .await
        .map_err(|e| format!("find entry {source}: {e}"))?
        .ok_or_else(|| format!("no journal entry booked for {source}"))?;
    if row.status != "posted" {
        return Err(format!("entry for {source} is {:?}, not posted", row.status));
    }
    posting::Entity::find()
        .filter(posting::Column::EntryId.eq(row.id))
        .all(db)
        .await
        .map_err(|e| format!("postings of {source}: {e}"))
}

async fn assert_leg(
    db: &DatabaseConnection,
    ps: &[posting::Model],
    role: &str,
    debit: Decimal,
    credit: Decimal,
    source: &str,
) -> Result<(), String> {
    let account_id = account::Entity::find()
        .filter(account::Column::SystemKey.eq(role))
        .one(db)
        .await
        .map_err(|e| format!("account for role {role}: {e}"))?
        .ok_or_else(|| format!("no account carries the role {role}"))?
        .id;
    let hits: Vec<&posting::Model> = ps.iter().filter(|p| p.account_id == account_id).collect();
    if hits.len() != 1 {
        return Err(format!(
            "{source}: expected one posting to {role}, found {}",
            hits.len()
        ));
    }
    let p = hits[0];
    if p.debit != debit || p.credit != credit {
        return Err(format!(
            "{source}: {role} booked Dr {} / Cr {}, expected Dr {debit} / Cr {credit}",
            p.debit, p.credit
        ));
    }
    Ok(())
}

/// The level row's (on_hand, value), zeros when the pair never moved.
async fn level_of(db: &DatabaseConnection, item_id: Uuid, warehouse_id: Uuid) -> (Decimal, Decimal) {
    stock::level::Entity::find_by_id((item_id, warehouse_id))
        .one(db)
        .await
        .unwrap()
        .map(|l| (l.on_hand, l.value))
        .unwrap_or((Decimal::ZERO, Decimal::ZERO))
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn dec(n: i64) -> Decimal {
    Decimal::from(n)
}

fn swap_database(url: &str, database: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database}"),
        None => format!("{url}/{database}"),
    }
}

fn line(item_id: Uuid, qty: i64, unit_price: i64, batch_id: Option<Uuid>) -> SaleLineInput {
    SaleLineInput {
        item_id,
        qty: dec(qty),
        unit_price: dec(unit_price),
        manual_price: false,
        discount_pct: None,
        batch_id,
    }
}

fn cash(amount: i64, tendered: Option<i64>) -> TenderInput {
    TenderInput {
        tender: "cash".into(),
        amount: dec(amount),
        tendered: tendered.map(dec),
        reference: None,
    }
}

fn card(amount: i64) -> TenderInput {
    TenderInput {
        tender: "card".into(),
        amount: dec(amount),
        tendered: None,
        reference: Some("SLIP-1".into()),
    }
}

fn simple_sale(session_id: Uuid, lines: Vec<SaleLineInput>, tenders: Vec<TenderInput>) -> NewSale {
    simple_sale_with_uuid(Uuid::new_v4(), session_id, lines, tenders)
}

fn simple_sale_with_uuid(
    client_uuid: Uuid,
    session_id: Uuid,
    lines: Vec<SaleLineInput>,
    tenders: Vec<TenderInput>,
) -> NewSale {
    NewSale {
        client_uuid,
        session_id,
        customer_id: None,
        sold_at: chrono::Utc::now(),
        captured_offline: false,
        lines,
        tenders,
        allow_override: false,
        capture_seconds: None,
        input_count: None,
        created_by: None,
    }
}

fn register_body(code: &str, name: &str, warehouse_id: Uuid) -> RegisterBody {
    RegisterBody {
        code: code.into(),
        name: name.into(),
        warehouse_id,
        price_list_id: None,
        default_customer_id: None,
        receipt_header: None,
        receipt_footer: None,
        allow_negative_stock_sales: false,
        is_active: true,
    }
}

fn stock_move(
    move_type: MoveType,
    from: Option<Uuid>,
    to: Option<Uuid>,
    lines: Vec<LineInput>,
) -> NewMove {
    NewMove {
        move_type,
        entry_date: chrono::Utc::now().date_naive(),
        memo: "pos test movement".into(),
        reference: None,
        from_warehouse_id: from,
        to_warehouse_id: to,
        lines,
        created_by: None,
    }
}

fn move_line(
    item_id: Uuid,
    qty: Decimal,
    unit_cost: Option<Decimal>,
    batch_no: Option<&str>,
) -> LineInput {
    LineInput {
        item_id,
        qty,
        unit_cost,
        entered_uom_id: None,
        batch_no: batch_no.map(str::to_string),
        serial_nos: None,
        memo: None,
    }
}

/// A minimal valid stockable item body.
fn item_body(sku: &str, name: &str, uom_id: Uuid) -> ItemBody {
    ItemBody {
        sku: sku.into(),
        name: name.into(),
        description: None,
        category_id: None,
        brand: None,
        manufacturer: None,
        manufacturer_part_no: None,
        model: None,
        barcode: None,
        image_file_id: None,
        country_of_origin: None,
        hs_code: None,
        notes: None,
        item_type: ItemType::Stockable,
        is_purchasable: true,
        is_sellable: true,
        is_active: true,
        uom_id,
        purchase_uom_id: None,
        sales_uom_id: None,
        purchase_uom_factor: None,
        costing_method: CostingMethod::MovingAverage,
        standard_cost: None,
        purchase_price: None,
        selling_price: None,
        min_selling_price: None,
        purchase_tax_code_id: None,
        sales_tax_code_id: None,
        preferred_supplier_id: None,
        lead_time_days: None,
        min_order_qty: None,
        order_multiple: None,
        reorder_level: None,
        reorder_qty: None,
        max_level: None,
        safety_stock: None,
        default_warehouse_id: None,
        track_batches: false,
        track_serials: false,
        shelf_life_days: None,
        warranty_days: None,
        allow_negative: false,
        weight: None,
        weight_uom_id: None,
        volume: None,
        length_mm: None,
        width_mm: None,
        height_mm: None,
        inventory_account_role: None,
        cogs_account_role: None,
        adjustment_account_role: None,
        expense_account_role: None,
    }
}
