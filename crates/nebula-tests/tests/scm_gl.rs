//! The GL posting port, wired end to end. The two apps that share it —
//! SCM as publisher, accounting as bookkeeper — boot into one kernel on a
//! throwaway database, and every posted SCM document is followed into the
//! ledger: the journal entry it produced, the accounts it hit, the amounts,
//! and the outbox row it must leave cleared once accounting acknowledges.
//!
//! This is the integration the per-app suites can't cover: `scm.rs` runs
//! with a subscriber-less bus (requests stage but never book), and
//! `accounting.rs` never publishes. Here the whole handshake runs in
//! process — `Events::publish` drives each subscriber to completion before
//! it returns — so the assertions need no sleeps.
//!
//! Skips when NEBULA_TEST_DATABASE_URL is unset.

use nebula::config::{Config, DatabaseConfig, MigrationsConfig};
use nebula::ports::gl::{GlLine, GlPostingRequested};
use nebula::{Kernel, Module, ModuleContext, Reset, SeriesDef, db};
use nebula_apps::accounting::gl_port::GlPort;
use nebula_apps::accounting::journal::{entry, posting};
use nebula_apps::accounting::{account, fiscal, seed as acc_seed};
use nebula_apps::scm::gl::{Gl, outbox, subscribe_acks};
use nebula_apps::scm::inventory::item::{
    self, CostingMethod, ItemBody, ItemType,
};
use nebula_apps::scm::inventory::moves::{LineInput, MoveService, MoveType, NewMove};
use nebula_apps::scm::inventory::warehouse;
use nebula_apps::scm::sales::customer::customer;
use nebula_apps::scm::sales::delivery::{DeliveryLineInput, DeliveryService, NewDelivery};
use nebula_apps::scm::sales::invoice::{
    InvoiceLineInput, InvoiceService, NewInvoice,
};
use nebula_apps::scm::sales::order::{NewOrder, OrderLineInput, OrderService};
use nebula_apps::scm::sales::payment::{NewPayment, PaymentAllocationInput, PaymentService};
use nebula_apps::scm::seed::{self as scm_seed, WALK_IN_CODE};
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

/// Wires both ends of the GL port onto one bus without registering either
/// full app — accounting's bookkeeper (`GlPort::subscribe`) and SCM's
/// outbox-ack subscriber (`subscribe_acks`), both public exactly so an
/// integration harness can. Sidesteps the apps' background workers (seed
/// rollouts, sweeper, auto-reorder) that would add nondeterminism; the
/// tables come from the SQL migrations under the root regardless. Declares
/// the number series the two sides pull from.
struct GlHarness;

impl Module for GlHarness {
    fn name(&self) -> &'static str {
        "scm-gl-harness"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        for (key, name, template) in [
            ("inventory.receipt", "Goods Receipt", "GRN-{YYYY}-{SEQ:5}"),
            ("inventory.issue", "Stock Issue", "ISS-{YYYY}-{SEQ:5}"),
            ("inventory.transfer", "Stock Transfer", "TRF-{YYYY}-{SEQ:5}"),
            ("inventory.adjustment", "Stock Adjustment", "ADJ-{YYYY}-{SEQ:5}"),
            ("sales.order", "Sales Order", "SO-{YYYY}-{SEQ:5}"),
            ("sales.delivery", "Delivery Note", "DN-{YYYY}-{SEQ:5}"),
            ("sales.invoice", "Sales Invoice", "SINV-{YYYY}-{SEQ:5}"),
            ("sales.credit_note", "Credit Note", "CN-{YYYY}-{SEQ:5}"),
            ("sales.payment", "Customer Payment", "RCT-{YYYY}-{SEQ:5}"),
            // The system series accounting books GL entries under.
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
        // Accounting's side: turn each posting request into a journal entry.
        GlPort::subscribe(ctx);
        // SCM's side: clear the outbox row when accounting answers.
        subscribe_acks(ctx);
    }
}

/// The whole scenario is one large future (the sales services build deep
/// ones); poll it from a thread with a generous stack, as `scm.rs` does.
#[test]
fn scm_postings_reach_the_ledger() {
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

    // A throwaway database: this test spans both accounting and SCM tables,
    // so it stays out of the shared `nebula_test` the per-app suites drop.
    let admin = db::connect(&DatabaseConfig {
        url: main_url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to create the test database");
    let fresh = format!("nebula_scm_gl_{}", Uuid::new_v4().simple());
    admin
        .execute_unprepared(&format!("CREATE DATABASE {fresh}"))
        .await
        .expect("must create the fresh database");

    let outcome = Box::pin(run(&swap_database(&main_url, &fresh))).await;

    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {fresh} WITH (FORCE)"))
        .await;

    outcome.expect("GL port flow must pass");
}

async fn run(url: &str) -> Result<(), String> {
    let mut config = Config::default();
    config.auth.jwt_secret = "scm-gl-test-secret".into();
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
        .add_module(GlHarness)
        .build()
        .map_err(|e| format!("kernel must build: {e}"))?
        .init()
        .await
        .map_err(|e| format!("boot must succeed: {e}"))?;
    let db = app.database().ok_or("database must exist")?.clone();

    // Seed both charts explicitly and synchronously — idempotent, so this
    // races harmlessly with accounting's own boot rollout and guarantees
    // the roles and an open fiscal year exist before the first posting.
    acc_seed::seed_defaults(&db, "USD")
        .await
        .map_err(|e| format!("accounting seed: {e}"))?;
    fiscal::FiscalService::new(db.clone())
        .ensure_current_year()
        .await
        .map_err(|e| format!("fiscal year seed: {e}"))?;
    scm_seed::seed_defaults(&db, "USD")
        .await
        .map_err(|e| format!("scm seed: {e}"))?;

    let numbering = app.numbering();
    let events = app.events();
    let gl = Gl::new(events.clone(), None);
    let today = chrono::Utc::now().date_naive();

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
    let moves = MoveService::new(db.clone());

    // =====================================================================
    // A. A direct stock receipt books Dr Inventory / Cr Stock adjustments.
    // =====================================================================
    let widget = items
        .create_item(item_body("GLW-1", "Ledger Widget", unit.id), None)
        .await
        .map_err(|e| format!("create item: {e}"))?;
    let receipt = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(widget.id, dec(10), Some(dec(100)))],
        ))
        .await
        .map_err(|e| format!("receipt draft: {e}"))?;
    let receipt = moves
        .post(receipt.id, &numbering, &gl)
        .await
        .map_err(|e| format!("receipt post: {e}"))?;

    let receipt_source = format!("scm.move:{}:post", receipt.id);
    let (entry_id, ps) = booked_entry(&db, &receipt_source).await?;
    assert_leg(&db, &ps, "inventory", dec(1000), Decimal::ZERO, &receipt_source).await?;
    assert_leg(
        &db,
        &ps,
        "stock_adjustment",
        Decimal::ZERO,
        dec(1000),
        &receipt_source,
    )
    .await?;
    assert_cleared(&db, &receipt_source).await?;
    ensure!(entry_id.is_some(), "receipt entry must have an id");

    // =====================================================================
    // B. An issue books Dr COGS / Cr Inventory at the moving average.
    // =====================================================================
    let issue = moves
        .create_draft(stock_move(
            MoveType::Issue,
            Some(main.id),
            None,
            vec![move_line(widget.id, dec(4), None)],
        ))
        .await
        .map_err(|e| format!("issue draft: {e}"))?;
    let issue = moves
        .post(issue.id, &numbering, &gl)
        .await
        .map_err(|e| format!("issue post: {e}"))?;
    let issue_source = format!("scm.move:{}:post", issue.id);
    let (_, ps) = booked_entry(&db, &issue_source).await?;
    assert_leg(&db, &ps, "cogs", dec(400), Decimal::ZERO, &issue_source).await?;
    assert_leg(&db, &ps, "inventory", Decimal::ZERO, dec(400), &issue_source).await?;
    assert_cleared(&db, &issue_source).await?;

    // =====================================================================
    // C. Idempotency: a re-emission of an already-booked source (what the
    //    sweeper does after a lost ack) books nothing twice.
    // =====================================================================
    events
        .publish(GlPostingRequested {
            tenant_id: None,
            source: receipt_source.clone(),
            entry_date: today,
            memo: "sweeper re-emission".into(),
            currency: None,
            lines: vec![
                GlLine {
                    account_role: "inventory".into(),
                    debit: dec(1000),
                    credit: Decimal::ZERO,
                    memo: None,
                },
                GlLine {
                    account_role: "stock_adjustment".into(),
                    debit: Decimal::ZERO,
                    credit: dec(1000),
                    memo: None,
                },
            ],
        })
        .await;
    let dupes = entry::Entity::find()
        .filter(entry::Column::Reference.eq(&receipt_source))
        .count(&db)
        .await
        .map_err(|e| format!("count entries: {e}"))?;
    ensure!(
        dupes == 1,
        "a re-emitted source must not double-book (found {dupes} entries for {receipt_source})"
    );

    // =====================================================================
    // D. The order-to-cash leg (sales.* sources) reaches the ledger too —
    //    COGS on delivery, AR on invoice, cash on payment — and each row
    //    clears. (These `sales.` sources are the ones the ack filter used
    //    to drop on the floor.)
    // =====================================================================
    let prod = items
        .create_item(item_body("GLS-1", "Ledger Sellable", unit.id), None)
        .await
        .map_err(|e| format!("create sellable: {e}"))?;
    let opening = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(prod.id, dec(100), Some(dec(30)))],
        ))
        .await
        .map_err(|e| format!("opening draft: {e}"))?;
    moves
        .post(opening.id, &numbering, &gl)
        .await
        .map_err(|e| format!("opening post: {e}"))?;

    let customer_id = customer::Entity::find()
        .filter(customer::Column::Code.eq(WALK_IN_CODE))
        .one(&db)
        .await
        .map_err(|e| format!("walk-in lookup: {e}"))?
        .ok_or("seeded walk-in customer")?
        .id;

    let orders = OrderService::new(db.clone());
    let so = orders
        .create_draft(NewOrder {
            customer_id,
            order_date: today,
            expected_date: None,
            warehouse_id: main.id,
            shipping_address: None,
            shipping_method: None,
            incoterms: None,
            customer_contact: None,
            customer_po_no: None,
            currency: None,
            payment_terms_days: None,
            tax_inclusive: false,
            discount_pct: None,
            discount_amount: None,
            other_charges: None,
            memo: None,
            terms_and_conditions: None,
            lines: vec![OrderLineInput {
                item_id: prod.id,
                description: None,
                qty: dec(10),
                warehouse_id: None,
                unit_price: Some(dec(50)),
                discount_pct: None,
                tax_code_id: None,
                expected_date: None,
                memo: None,
            }],
            created_by: None,
            allow_price_override: true,
        })
        .await
        .map_err(|e| format!("so draft: {e}"))?;
    // The seeded walk-in customer is cash-only (credit limit 0); override
    // the credit check to place a standing order for the GL walk.
    let so = orders
        .confirm(so.id, None, None, true, &numbering)
        .await
        .map_err(|e| format!("so confirm: {e}"))?;
    let so_line_id = so.lines[0].id;

    // Deliver 6 → Dr COGS / Cr Inventory 180 (6 × 30).
    let deliveries = DeliveryService::new(db.clone());
    let dn = deliveries
        .create_draft(NewDelivery {
            order_id: so.id,
            delivery_date: today,
            carrier: None,
            tracking_no: None,
            vehicle_reg: None,
            driver_name: None,
            received_by_name: None,
            shipping_address: None,
            memo: None,
            lines: vec![DeliveryLineInput {
                order_line_id: so_line_id,
                qty: dec(6),
                batch_no: None,
                serial_nos: None,
                memo: None,
            }],
            created_by: None,
        })
        .await
        .map_err(|e| format!("dn draft: {e}"))?;
    let dn = deliveries
        .post(dn.id, &numbering, &gl)
        .await
        .map_err(|e| format!("dn post: {e}"))?;
    let dn_source = format!("sales.delivery:{}:post", dn.id);
    let (_, ps) = booked_entry(&db, &dn_source).await?;
    assert_leg(&db, &ps, "cogs", dec(180), Decimal::ZERO, &dn_source).await?;
    assert_leg(&db, &ps, "inventory", Decimal::ZERO, dec(180), &dn_source).await?;
    assert_cleared(&db, &dn_source).await?;

    // Invoice 6 → Dr AR / Cr Sales 300 (walk-in carries no tax code).
    let invoices = InvoiceService::new(db.clone());
    let inv = invoices
        .create_draft(NewInvoice {
            order_id: so.id,
            invoice_date: today,
            due_date: None,
            payment_terms_days: None,
            exchange_rate: None,
            tax_inclusive: false,
            discount_pct: None,
            discount_amount: None,
            other_charges: None,
            customer_po_no: None,
            attachment_file_id: None,
            memo: None,
            lines: vec![InvoiceLineInput {
                order_line_id: Some(so_line_id),
                description: None,
                qty: dec(6),
                unit_price: dec(50),
                discount_pct: None,
                tax_code_id: None,
                memo: None,
            }],
            created_by: None,
        })
        .await
        .map_err(|e| format!("inv draft: {e}"))?;
    let inv = invoices
        .post(inv.id, &numbering, None, &gl)
        .await
        .map_err(|e| format!("inv post: {e}"))?;
    let inv_source = format!("sales.invoice:{}:post", inv.id);
    let (_, ps) = booked_entry(&db, &inv_source).await?;
    assert_leg(&db, &ps, "ar", dec(300), Decimal::ZERO, &inv_source).await?;
    assert_leg(&db, &ps, "sales", Decimal::ZERO, dec(300), &inv_source).await?;
    assert_cleared(&db, &inv_source).await?;

    // Pay 300 by bank transfer → Dr Bank / Cr AR 300.
    let payments = PaymentService::new(db.clone());
    let pay = payments
        .create_draft(NewPayment {
            customer_id,
            payment_date: today,
            method: "bank_transfer".into(),
            reference: None,
            currency: None,
            exchange_rate: None,
            amount: dec(300),
            memo: None,
            allocations: vec![PaymentAllocationInput {
                invoice_id: inv.id,
                amount: dec(300),
            }],
            created_by: None,
        })
        .await
        .map_err(|e| format!("pay draft: {e}"))?;
    let pay = payments
        .post(pay.id, &numbering, None, &gl)
        .await
        .map_err(|e| format!("pay post: {e}"))?;
    let pay_source = format!("sales.payment:{}:post", pay.id);
    let (_, ps) = booked_entry(&db, &pay_source).await?;
    assert_leg(&db, &ps, "bank", dec(300), Decimal::ZERO, &pay_source).await?;
    assert_leg(&db, &ps, "ar", Decimal::ZERO, dec(300), &pay_source).await?;
    assert_cleared(&db, &pay_source).await?;

    // =====================================================================
    // E. Global invariants: the whole ledger balances, and the outbox has
    //    fully drained — every staged request was acknowledged and cleared.
    // =====================================================================
    let all = posting::Entity::find()
        .all(&db)
        .await
        .map_err(|e| format!("all postings: {e}"))?;
    let debits: Decimal = all.iter().map(|p| p.debit).sum();
    let credits: Decimal = all.iter().map(|p| p.credit).sum();
    ensure!(
        debits == credits,
        "double entry must hold across the ledger: debits {debits} != credits {credits}"
    );
    ensure!(debits > Decimal::ZERO, "the cycle booked something");

    let staged = outbox::Entity::find()
        .count(&db)
        .await
        .map_err(|e| format!("outbox count: {e}"))?;
    ensure!(
        staged == 0,
        "every posting must be acknowledged and cleared; {staged} rows linger in the outbox"
    );

    Ok(())
}

/// Fetch the posted journal entry a source produced, with its postings.
async fn booked_entry(
    db: &DatabaseConnection,
    source: &str,
) -> Result<(Option<Uuid>, Vec<posting::Model>), String> {
    let row = entry::Entity::find()
        .filter(entry::Column::Reference.eq(source))
        .one(db)
        .await
        .map_err(|e| format!("find entry {source}: {e}"))?
        .ok_or_else(|| format!("no journal entry booked for {source}"))?;
    if row.status != "posted" {
        return Err(format!("entry for {source} is {:?}, not posted", row.status));
    }
    let ps = posting::Entity::find()
        .filter(posting::Column::EntryId.eq(row.id))
        .all(db)
        .await
        .map_err(|e| format!("postings of {source}: {e}"))?;
    Ok((Some(row.id), ps))
}

/// Assert the entry has exactly one posting to `role`'s account carrying the
/// given debit and credit.
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

/// Assert the source's outbox row is gone — accounting acknowledged it and
/// SCM cleared it.
async fn assert_cleared(db: &DatabaseConnection, source: &str) -> Result<(), String> {
    let row = outbox::Entity::find_by_id(source.to_string())
        .one(db)
        .await
        .map_err(|e| format!("outbox lookup {source}: {e}"))?;
    ensure!(
        row.is_none(),
        "outbox row for {source} was booked but never cleared"
    );
    Ok(())
}

fn dec(n: i64) -> Decimal {
    Decimal::from(n)
}

fn swap_database(url: &str, database: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database}"),
        None => format!("{url}/{database}"),
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
        memo: "gl port test".into(),
        reference: None,
        from_warehouse_id: from,
        to_warehouse_id: to,
        lines,
        created_by: None,
    }
}

fn move_line(item_id: Uuid, qty: Decimal, unit_cost: Option<Decimal>) -> LineInput {
    LineInput {
        item_id,
        qty,
        unit_cost,
        entered_uom_id: None,
        batch_no: None,
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
