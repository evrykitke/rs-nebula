//! The SCM app's invariants against a live database: seeding, the item /
//! warehouse masters, the stock engine's moving-average math and locking,
//! and the purchase-to-pay cycle. Skips when NEBULA_TEST_DATABASE_URL is
//! unset.
//!
//! One test drives every case: the scm tables are shared per database, so
//! splitting into parallel tests would race on the seed. Concurrency is
//! still tested — race sections spawn tasks on clones of the pool *inside*
//! the one test, so the contention is real (separate connections, separate
//! transactions) while the harness stays sequential.

use nebula::config::{Config, DatabaseConfig, MigrationsConfig};
use nebula::{Events, Kernel, Module, ModuleContext, Reset, SeriesDef};
use nebula_apps::scm::gl::Gl;
use nebula_apps::scm::inventory::item::{self, CostingMethod, ItemBody, ItemType, UomBody};
use nebula_apps::scm::inventory::moves::{LineInput, MoveService, MoveStatus, MoveType, NewMove};
use nebula_apps::scm::inventory::stock;
use nebula_apps::scm::inventory::warehouse::{self, WarehouseBody};
use nebula_apps::scm::procurement::invoice::{
    InvoiceLineInput, InvoiceService, InvoiceStatus, NewInvoice,
};
use nebula_apps::scm::procurement::order::{
    NewOrder, OrderLineInput, OrderService, OrderStatus,
};
use nebula_apps::scm::procurement::receipt::{
    NewReceipt, ReceiptLineInput, ReceiptService, ReceiptStatus,
};
use nebula_apps::scm::procurement::reports::ProcurementQueries;
use nebula_apps::scm::procurement::supplier::{ItemSupplierBody, SupplierBody, Store as SupplierStore};
use nebula_apps::scm::seed;
use rust_decimal::Decimal;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter};
use uuid::Uuid;

/// Declares the scm number series without registering the whole app, so
/// seeding is driven explicitly by the test instead of a background
/// rollout task.
struct SeriesOnly;

impl Module for SeriesOnly {
    fn name(&self) -> &'static str {
        "scm-series-test"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        for (key, name, template) in [
            ("inventory.receipt", "Goods Receipt", "GRN-{YYYY}-{SEQ:5}"),
            ("inventory.issue", "Stock Issue", "ISS-{YYYY}-{SEQ:5}"),
            ("inventory.transfer", "Stock Transfer", "TRF-{YYYY}-{SEQ:5}"),
            (
                "inventory.adjustment",
                "Stock Adjustment",
                "ADJ-{YYYY}-{SEQ:5}",
            ),
            ("procurement.order", "Purchase Order", "PO-{YYYY}-{SEQ:5}"),
            (
                "procurement.invoice",
                "Purchase Invoice",
                "PINV-{YYYY}-{SEQ:5}",
            ),
        ] {
            ctx.declare_series(
                SeriesDef::new(key, name, template, Reset::Yearly)
                    .expect("valid series template"),
            );
        }
    }
}

/// A minimal valid item body; tests tweak the fields they care about.
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

/// A movement document input; tests pick the type and warehouses.
fn stock_move(
    move_type: MoveType,
    from: Option<Uuid>,
    to: Option<Uuid>,
    lines: Vec<LineInput>,
) -> NewMove {
    NewMove {
        move_type,
        entry_date: chrono::Utc::now().date_naive(),
        memo: "test movement".into(),
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

/// The level row's (on_hand, value), zeros when the pair never moved.
async fn level_of(db: &DatabaseConnection, item_id: Uuid, warehouse_id: Uuid) -> (Decimal, Decimal) {
    stock::level::Entity::find_by_id((item_id, warehouse_id))
        .one(db)
        .await
        .unwrap()
        .map(|l| (l.on_hand, l.value))
        .unwrap_or((Decimal::ZERO, Decimal::ZERO))
}

fn warehouse_body(code: &str, name: &str) -> WarehouseBody {
    WarehouseBody {
        code: code.into(),
        name: name.into(),
        warehouse_type: "standard".into(),
        parent_id: None,
        address_line1: None,
        address_line2: None,
        city: None,
        region: None,
        postal_code: None,
        country: None,
        phone: None,
        email: None,
        contact_name: None,
        is_default: false,
        allow_negative: false,
        is_active: true,
        notes: None,
    }
}

/// A minimal valid supplier; tests tweak the fields they care about.
fn supplier_body(code: &str, name: &str, currency: &str) -> SupplierBody {
    SupplierBody {
        code: code.into(),
        name: name.into(),
        legal_name: None,
        supplier_type: "company".into(),
        registration_no: None,
        tax_number: None,
        industry: None,
        website: None,
        contact_name: None,
        email: None,
        phone: None,
        secondary_contact_name: None,
        secondary_email: None,
        secondary_phone: None,
        address_line1: None,
        address_line2: None,
        city: None,
        region: None,
        postal_code: None,
        country: None,
        currency: currency.into(),
        payment_terms_days: 30,
        credit_limit: None,
        default_discount_pct: None,
        default_tax_code_id: None,
        incoterms: None,
        lead_time_days: None,
        min_order_value: None,
        bank_name: None,
        bank_branch: None,
        bank_account_name: None,
        bank_account_no: None,
        bank_swift: None,
        mobile_money_no: None,
        payment_notes: None,
        is_preferred: false,
        on_hold: false,
        hold_reason: None,
        is_active: true,
        notes: None,
    }
}

fn purchase_order(
    supplier_id: Uuid,
    warehouse_id: Uuid,
    lines: Vec<OrderLineInput>,
) -> NewOrder {
    NewOrder {
        supplier_id,
        order_date: chrono::Utc::now().date_naive(),
        expected_date: None,
        deliver_to_warehouse_id: warehouse_id,
        delivery_address: None,
        shipping_method: None,
        incoterms: None,
        supplier_contact: None,
        currency: None,
        payment_terms_days: None,
        tax_inclusive: false,
        discount_pct: None,
        discount_amount: None,
        other_charges: None,
        memo: None,
        reference: None,
        terms_and_conditions: None,
        lines,
        created_by: None,
    }
}

fn po_line(item_id: Uuid, qty: Decimal, unit_price: Decimal) -> OrderLineInput {
    OrderLineInput {
        item_id,
        description: None,
        qty,
        unit_price,
        discount_pct: None,
        tax_code_id: None,
        expected_date: None,
        memo: None,
    }
}

fn goods_receipt(order_id: Uuid, lines: Vec<ReceiptLineInput>) -> NewReceipt {
    NewReceipt {
        order_id,
        receipt_date: chrono::Utc::now().date_naive(),
        reference: None,
        carrier: None,
        tracking_no: None,
        vehicle_reg: None,
        delivered_by: None,
        exchange_rate: None,
        memo: None,
        lines,
        created_by: None,
    }
}

fn gr_line(order_line_id: Uuid, qty: Decimal) -> ReceiptLineInput {
    ReceiptLineInput {
        order_line_id,
        qty,
        rejected_qty: None,
        reject_reason: None,
        batch_no: None,
        serial_nos: None,
        memo: None,
    }
}

fn vendor_bill(
    supplier_id: Uuid,
    order_id: Uuid,
    supplier_invoice_no: &str,
    lines: Vec<InvoiceLineInput>,
) -> NewInvoice {
    NewInvoice {
        supplier_id,
        order_id,
        supplier_invoice_no: supplier_invoice_no.into(),
        invoice_date: chrono::Utc::now().date_naive(),
        due_date: None,
        payment_terms_days: None,
        exchange_rate: None,
        tax_inclusive: false,
        discount_pct: None,
        discount_amount: None,
        other_charges: None,
        attachment_file_id: None,
        memo: None,
        lines,
        created_by: None,
    }
}

fn bill_line(order_line_id: Uuid, qty: Decimal, unit_price: Decimal) -> InvoiceLineInput {
    InvoiceLineInput {
        order_line_id,
        description: None,
        qty,
        unit_price,
        discount_pct: None,
        tax_code_id: None,
        memo: None,
    }
}

/// The level row's `on_order` (zero when the pair never moved).
async fn on_order_of(db: &DatabaseConnection, item_id: Uuid, warehouse_id: Uuid) -> Decimal {
    stock::level::Entity::find_by_id((item_id, warehouse_id))
        .one(db)
        .await
        .unwrap()
        .map(|l| l.on_order)
        .unwrap_or(Decimal::ZERO)
}

/// The whole suite is one async scenario, and in a debug build its future
/// (every local across every await) is bigger than a default test thread's
/// stack. Poll it from a thread with room instead.
#[test]
fn scm_end_to_end() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime must build")
                .block_on(scenario());
        })
        .expect("test thread must spawn")
        .join()
        .expect("test thread must not panic");
}

async fn scenario() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    // Clean slate for the scm module only; the framework schema is
    // idempotent under auto_migrate. Children first, so plain DROPs work.
    let admin_db = nebula::db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");
    admin_db
        .execute_unprepared(
            "DROP TABLE IF EXISTS scm_gl_outbox; \
             DROP TABLE IF EXISTS procurement_rfq_quotes; \
             DROP TABLE IF EXISTS procurement_rfq_suppliers; \
             DROP TABLE IF EXISTS procurement_rfq_lines; \
             DROP TABLE IF EXISTS procurement_rfqs; \
             DROP TABLE IF EXISTS procurement_requisition_lines; \
             DROP TABLE IF EXISTS procurement_requisitions; \
             DROP TABLE IF EXISTS procurement_return_lines; \
             DROP TABLE IF EXISTS procurement_returns; \
             DROP TABLE IF EXISTS procurement_invoice_lines; \
             DROP TABLE IF EXISTS procurement_invoices; \
             DROP TABLE IF EXISTS procurement_receipt_lines; \
             DROP TABLE IF EXISTS procurement_receipts; \
             DROP TABLE IF EXISTS procurement_order_lines; \
             DROP TABLE IF EXISTS procurement_orders; \
             DROP TABLE IF EXISTS procurement_item_suppliers; \
             DROP TABLE IF EXISTS procurement_suppliers; \
             DROP TABLE IF EXISTS inventory_stock_ledger; \
             DROP TABLE IF EXISTS inventory_stock_levels; \
             DROP TABLE IF EXISTS inventory_move_line_serials; \
             DROP TABLE IF EXISTS inventory_move_lines; \
             DROP TABLE IF EXISTS inventory_serials; \
             DROP TABLE IF EXISTS inventory_batches; \
             DROP TABLE IF EXISTS inventory_moves; \
             DROP TABLE IF EXISTS inventory_item_barcodes; \
             DROP TABLE IF EXISTS inventory_items; \
             DROP TABLE IF EXISTS inventory_uom_conversions; \
             DROP TABLE IF EXISTS inventory_uoms; \
             DROP TABLE IF EXISTS inventory_categories; \
             DROP TABLE IF EXISTS inventory_warehouses; \
             DO $$ BEGIN IF to_regclass('public.nebula_sql_migrations') IS NOT NULL THEN \
               DELETE FROM nebula_sql_migrations WHERE module = 'scm'; \
             END IF; END $$;",
        )
        .await
        .expect("cleanup must work");

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    // The repo's real migration files, wherever the test runs from.
    config.migrations = MigrationsConfig {
        root: format!("{}/../../migrations", env!("CARGO_MANIFEST_DIR")),
    };

    let app = Kernel::builder()
        .with_config(config)
        .add_module(SeriesOnly)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot must succeed");
    let db = app.database().expect("database must exist").clone();

    // --- seeding: once, then a no-op ---
    assert!(seed::seed_defaults(&db).await.unwrap());
    assert!(
        !seed::seed_defaults(&db).await.unwrap(),
        "second seed must be a no-op"
    );

    let warehouses = warehouse::Store::new(db.clone());
    let main = warehouses
        .find_all()
        .await
        .unwrap()
        .into_iter()
        .find(|w| w.code == "MAIN")
        .expect("seeded Main warehouse");
    assert!(main.is_default, "seeded warehouse is the default");

    let items = item::Store::new(db.clone());
    let uoms = items.list_uoms().await.unwrap();
    assert_eq!(uoms.len(), 8, "eight starter UoMs");
    let unit = uoms.iter().find(|u| u.code == "unit").unwrap().clone();
    let kg = uoms.iter().find(|u| u.code == "kg").unwrap().clone();
    assert!(!unit.fractional && kg.fractional);

    // --- uom uniqueness ---
    let dup = items
        .create_uom(UomBody {
            code: "unit".into(),
            name: "Duplicate".into(),
            symbol: None,
            fractional: false,
        })
        .await;
    assert!(dup.is_err(), "duplicate uom code must be rejected");

    // --- item CRUD ---
    let widget = items
        .create_item(item_body("WID-1", "Widget", unit.id), None)
        .await
        .unwrap();
    assert_eq!(widget.costing_method, "moving_average");

    let dup = items.create_item(item_body("WID-1", "Other", unit.id), None).await;
    assert!(dup.is_err(), "duplicate sku must be rejected");

    let bad_uom = items
        .create_item(item_body("WID-2", "Widget 2", Uuid::new_v4()), None)
        .await;
    assert!(bad_uom.is_err(), "unknown uom must be rejected");

    let mut fifo = item_body("WID-3", "Widget 3", unit.id);
    fifo.costing_method = CostingMethod::Fifo;
    assert!(
        items.create_item(fifo, None).await.is_err(),
        "only moving_average is accepted for now"
    );

    let mut renamed = item_body("WID-1", "Widget Mk II", unit.id);
    renamed.selling_price = Some(rust_decimal::Decimal::from(150));
    let updated = items.update_item(widget.id, renamed, None).await.unwrap();
    assert_eq!(updated.name, "Widget Mk II");

    // --- categories: cycle rejection ---
    let parent = items
        .create_category(
            nebula_apps::scm::inventory::item::CategoryBody {
                code: None,
                name: "Electronics".into(),
                description: None,
                parent_id: None,
                default_costing_method: None,
                default_uom_id: None,
                inventory_account_role: None,
                cogs_account_role: None,
                adjustment_account_role: None,
                is_active: true,
            },
            None,
        )
        .await
        .unwrap();
    let child = items
        .create_category(
            nebula_apps::scm::inventory::item::CategoryBody {
                code: None,
                name: "Phones".into(),
                description: None,
                parent_id: Some(parent.id),
                default_costing_method: None,
                default_uom_id: None,
                inventory_account_role: None,
                cogs_account_role: None,
                adjustment_account_role: None,
                is_active: true,
            },
            None,
        )
        .await
        .unwrap();
    let cycle = items
        .update_category(
            parent.id,
            nebula_apps::scm::inventory::item::CategoryBody {
                code: None,
                name: "Electronics".into(),
                description: None,
                parent_id: Some(child.id),
                default_costing_method: None,
                default_uom_id: None,
                inventory_account_role: None,
                cogs_account_role: None,
                adjustment_account_role: None,
                is_active: true,
            },
            None,
        )
        .await;
    assert!(cycle.is_err(), "category cycle must be rejected");

    // --- single default warehouse ---
    let mut second = warehouse_body("WH2", "Second");
    second.is_default = true;
    let wh2 = warehouses.create(second, None).await.unwrap();
    assert!(wh2.is_default);
    let main_after = warehouses.find_by_id(main.id).await.unwrap();
    assert!(
        !main_after.is_default,
        "claiming the default must demote the previous one"
    );
    let count_default = warehouse::Entity::find()
        .filter(warehouse::Column::IsDefault.eq(true))
        .all(&db)
        .await
        .unwrap()
        .len();
    assert_eq!(count_default, 1, "at most one default warehouse");

    let dup = warehouses.create(warehouse_body("WH2", "Clash"), None).await;
    assert!(dup.is_err(), "duplicate warehouse code must be rejected");

    // =======================================================================
    // Phase 2: the stock engine and movement documents
    // =======================================================================
    let numbering = app.numbering();
    let moves = MoveService::new(db.clone());
    // A bus with no subscribers: publishes are no-ops, staging still runs.
    let gl = Gl::new(Events::new(), None);

    // --- the moving-average worked example (exact decimals, no epsilons) ---
    let stk = items
        .create_item(item_body("STK-1", "Engine Widget", unit.id), None)
        .await
        .unwrap();

    let r1 = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(stk.id, Decimal::from(10), Some(Decimal::from(100)))],
        ))
        .await
        .unwrap();
    assert_eq!(r1.status, MoveStatus::Draft);
    assert!(r1.number.is_none(), "drafts are unnumbered");
    let r1 = moves.post(r1.id, &numbering, &gl).await.unwrap();
    assert_eq!(r1.status, MoveStatus::Posted);
    assert!(
        r1.number.as_deref().unwrap().starts_with("GRN-"),
        "receipts number from the GRN series"
    );
    assert_eq!(
        level_of(&db, stk.id, main.id).await,
        (Decimal::from(10), Decimal::from(1000))
    );

    let r2 = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(stk.id, Decimal::from(10), Some(Decimal::from(200)))],
        ))
        .await
        .unwrap();
    moves.post(r2.id, &numbering, &gl).await.unwrap();
    assert_eq!(
        level_of(&db, stk.id, main.id).await,
        (Decimal::from(20), Decimal::from(3000)),
        "10@100 + 10@200 = 20 on hand worth 3000 (avg 150)"
    );

    let i1 = moves
        .create_draft(stock_move(
            MoveType::Issue,
            Some(main.id),
            None,
            vec![move_line(stk.id, Decimal::from(5), None)],
        ))
        .await
        .unwrap();
    let i1 = moves.post(i1.id, &numbering, &gl).await.unwrap();
    assert!(i1.number.as_deref().unwrap().starts_with("ISS-"));
    let i1_rows = stock::ledger::Entity::find()
        .filter(stock::ledger::Column::MoveId.eq(i1.id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(i1_rows.len(), 1);
    assert_eq!(i1_rows[0].unit_cost, Decimal::from(150), "issued at the average");
    assert_eq!(i1_rows[0].value_delta, Decimal::from(-750));
    assert_eq!(
        level_of(&db, stk.id, main.id).await,
        (Decimal::from(15), Decimal::from(2250))
    );

    // Emptying the location flushes the whole remaining value.
    let i2 = moves
        .create_draft(stock_move(
            MoveType::Issue,
            Some(main.id),
            None,
            vec![move_line(stk.id, Decimal::from(15), None)],
        ))
        .await
        .unwrap();
    moves.post(i2.id, &numbering, &gl).await.unwrap();
    let (on_hand, value) = level_of(&db, stk.id, main.id).await;
    assert_eq!(on_hand, Decimal::ZERO);
    assert_eq!(value, Decimal::ZERO, "zero quantity means exactly zero value");

    // --- ledger replay == level (the engine's core invariant) ---
    let rows = stock::ledger::Entity::find()
        .filter(stock::ledger::Column::ItemId.eq(stk.id))
        .filter(stock::ledger::Column::WarehouseId.eq(main.id))
        .all(&db)
        .await
        .unwrap();
    let replay_qty: Decimal = rows.iter().map(|r| r.qty_delta).sum();
    let replay_value: Decimal = rows.iter().map(|r| r.value_delta).sum();
    assert_eq!((replay_qty, replay_value), level_of(&db, stk.id, main.id).await);

    // --- rejections ---
    let short = moves
        .create_draft(stock_move(
            MoveType::Issue,
            Some(main.id),
            None,
            vec![move_line(stk.id, Decimal::ONE, None)],
        ))
        .await
        .unwrap();
    let err = moves.post(short.id, &numbering, &gl).await.unwrap_err();
    assert!(err.to_string().contains("insufficient stock"));

    let fractional = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(stk.id, "0.5".parse().unwrap(), Some(Decimal::from(10)))],
        ))
        .await
        .unwrap();
    assert!(
        moves.post(fractional.id, &numbering, &gl).await.is_err(),
        "fractional qty on a whole-unit UoM must be rejected"
    );

    let costless = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(stk.id, Decimal::from(2), None)],
        ))
        .await
        .unwrap();
    assert!(
        moves.post(costless.id, &numbering, &gl).await.is_err(),
        "a receipt line needs a unit cost at post"
    );

    assert!(
        moves
            .create_draft(stock_move(
                MoveType::Issue,
                Some(main.id),
                None,
                vec![move_line(stk.id, Decimal::ONE, Some(Decimal::from(5)))],
            ))
            .await
            .is_err(),
        "issues never take a caller cost"
    );
    assert!(
        moves
            .create_draft(stock_move(
                MoveType::Transfer,
                Some(main.id),
                Some(main.id),
                vec![move_line(stk.id, Decimal::ONE, None)],
            ))
            .await
            .is_err(),
        "a transfer needs two different warehouses"
    );

    let mut svc_item = item_body("SVC-1", "Consulting", unit.id);
    svc_item.item_type = ItemType::Service;
    let svc_item = items.create_item(svc_item, None).await.unwrap();
    let svc_receipt = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(svc_item.id, Decimal::ONE, Some(Decimal::from(10)))],
        ))
        .await
        .unwrap();
    assert!(
        moves.post(svc_receipt.id, &numbering, &gl).await.is_err(),
        "a service item never moves through the ledger"
    );

    // Deleting an item with stock history deactivates instead; drafts on
    // an inactive item are refused.
    let deleted = items.delete_item(stk.id).await.unwrap();
    assert!(!deleted.is_active, "moved items deactivate on delete");
    assert!(items.find_item(stk.id).await.is_ok(), "the row survives");
    assert!(
        moves
            .create_draft(stock_move(
                MoveType::Receipt,
                None,
                Some(main.id),
                vec![move_line(stk.id, Decimal::ONE, Some(Decimal::from(1)))],
            ))
            .await
            .is_err(),
        "inactive items cannot move"
    );

    // --- transfers preserve value ---
    let xfr = items
        .create_item(item_body("XFR-1", "Transfer Widget", unit.id), None)
        .await
        .unwrap();
    let seed_receipt = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(xfr.id, Decimal::from(10), Some(Decimal::from(150)))],
        ))
        .await
        .unwrap();
    moves.post(seed_receipt.id, &numbering, &gl).await.unwrap();
    let transfer = moves
        .create_draft(stock_move(
            MoveType::Transfer,
            Some(main.id),
            Some(wh2.id),
            vec![move_line(xfr.id, Decimal::from(4), None)],
        ))
        .await
        .unwrap();
    let transfer = moves.post(transfer.id, &numbering, &gl).await.unwrap();
    assert!(transfer.number.as_deref().unwrap().starts_with("TRF-"));
    assert_eq!(
        level_of(&db, xfr.id, main.id).await,
        (Decimal::from(6), Decimal::from(900))
    );
    assert_eq!(
        level_of(&db, xfr.id, wh2.id).await,
        (Decimal::from(4), Decimal::from(600)),
        "a transfer moves value 1:1 at the issued cost"
    );

    // --- adjustments carry the counted quantity ---
    let adj_item = items
        .create_item(item_body("ADJ-1", "Counted Widget", unit.id), None)
        .await
        .unwrap();
    let from_zero = moves
        .create_draft(stock_move(
            MoveType::Adjustment,
            Some(wh2.id),
            None,
            vec![move_line(adj_item.id, Decimal::from(5), None)],
        ))
        .await
        .unwrap();
    assert!(
        moves.post(from_zero.id, &numbering, &gl).await.is_err(),
        "counting up from zero stock needs a unit cost"
    );
    let opening = moves
        .create_draft(stock_move(
            MoveType::Adjustment,
            Some(wh2.id),
            None,
            vec![move_line(adj_item.id, Decimal::from(5), Some(Decimal::from(20)))],
        ))
        .await
        .unwrap();
    let opening = moves.post(opening.id, &numbering, &gl).await.unwrap();
    assert!(opening.number.as_deref().unwrap().starts_with("ADJ-"));
    assert_eq!(
        level_of(&db, adj_item.id, wh2.id).await,
        (Decimal::from(5), Decimal::from(100)),
        "opening stock is an adjustment with a cost"
    );

    let count_down = moves
        .create_draft(stock_move(
            MoveType::Adjustment,
            Some(wh2.id),
            None,
            vec![move_line(adj_item.id, Decimal::from(2), None)],
        ))
        .await
        .unwrap();
    moves.post(count_down.id, &numbering, &gl).await.unwrap();
    assert_eq!(
        level_of(&db, adj_item.id, wh2.id).await,
        (Decimal::from(2), Decimal::from(40)),
        "counting down issues the difference at the average"
    );

    let confirming = moves
        .create_draft(stock_move(
            MoveType::Adjustment,
            Some(wh2.id),
            None,
            vec![move_line(adj_item.id, Decimal::from(2), None)],
        ))
        .await
        .unwrap();
    let confirming = moves.post(confirming.id, &numbering, &gl).await.unwrap();
    let confirming_rows = stock::ledger::Entity::find()
        .filter(stock::ledger::Column::MoveId.eq(confirming.id))
        .all(&db)
        .await
        .unwrap();
    assert!(
        confirming_rows.is_empty(),
        "a count that matches on-hand books nothing"
    );

    // --- reversals mirror through the engine ---
    let rev = items
        .create_item(item_body("REV-1", "Reversible Widget", unit.id), None)
        .await
        .unwrap();
    let rev_receipt = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(rev.id, Decimal::from(3), Some(Decimal::from(10)))],
        ))
        .await
        .unwrap();
    let rev_receipt = moves.post(rev_receipt.id, &numbering, &gl).await.unwrap();
    let rev_issue = moves
        .create_draft(stock_move(
            MoveType::Issue,
            Some(main.id),
            None,
            vec![move_line(rev.id, Decimal::from(2), None)],
        ))
        .await
        .unwrap();
    let rev_issue = moves.post(rev_issue.id, &numbering, &gl).await.unwrap();

    let err = moves
        .reverse(rev_receipt.id, "undo", None, &numbering, None, &gl)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("insufficient stock"),
        "a receipt whose stock has since been issued cannot reverse"
    );

    let issue_reversal = moves
        .reverse(rev_issue.id, "wrong item", None, &numbering, None, &gl)
        .await
        .unwrap();
    assert_eq!(issue_reversal.reverses_id, Some(rev_issue.id));
    assert_eq!(
        level_of(&db, rev.id, main.id).await,
        (Decimal::from(3), Decimal::from(30)),
        "reversing the issue puts the stock back at the cost it left"
    );
    let original = moves.view(rev_issue.id).await.unwrap();
    assert_eq!(original.status, MoveStatus::Reversed);
    assert_eq!(original.reversed_by_id, Some(issue_reversal.id));
    assert!(
        moves.reverse(rev_issue.id, "again", None, &numbering, None, &gl).await.is_err(),
        "a movement reverses once"
    );

    moves
        .reverse(rev_receipt.id, "undo", None, &numbering, None, &gl)
        .await
        .unwrap();
    assert_eq!(
        level_of(&db, rev.id, main.id).await,
        (Decimal::ZERO, Decimal::ZERO)
    );

    let draft = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(rev.id, Decimal::ONE, Some(Decimal::from(1)))],
        ))
        .await
        .unwrap();
    assert!(
        moves.reverse(draft.id, "not posted", None, &numbering, None, &gl).await.is_err(),
        "drafts cannot be reversed"
    );
    moves.delete_draft(draft.id).await.unwrap();

    // --- race test A: two concurrent issues of the last unit ---
    // Real contention: separate connections from the shared pool, separate
    // transactions, both queued on the same level row lock. Exactly one may
    // win; never both, never neither.
    let race = items
        .create_item(item_body("RACE-1", "Contended Widget", unit.id), None)
        .await
        .unwrap();
    let last_unit = moves
        .create_draft(stock_move(
            MoveType::Receipt,
            None,
            Some(main.id),
            vec![move_line(race.id, Decimal::ONE, Some(Decimal::from(10)))],
        ))
        .await
        .unwrap();
    moves.post(last_unit.id, &numbering, &gl).await.unwrap();

    let mut race_drafts = Vec::new();
    for _ in 0..2 {
        let draft = moves
            .create_draft(stock_move(
                MoveType::Issue,
                Some(main.id),
                None,
                vec![move_line(race.id, Decimal::ONE, None)],
            ))
            .await
            .unwrap();
        race_drafts.push(draft.id);
    }
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for draft_id in race_drafts {
        let db = db.clone();
        let numbering = numbering.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let gl = Gl::new(Events::new(), None);
            barrier.wait().await;
            MoveService::new(db).post(draft_id, &numbering, &gl).await
        }));
    }
    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.expect("race task must not panic"));
    }
    let wins = outcomes.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "exactly one concurrent issue of the last unit succeeds");
    let loss = outcomes.iter().find(|r| r.is_err()).unwrap();
    assert!(
        loss.as_ref().unwrap_err().to_string().contains("insufficient stock"),
        "the loser fails the negative-stock check, not a deadlock"
    );
    assert_eq!(
        level_of(&db, race.id, main.id).await,
        (Decimal::ZERO, Decimal::ZERO)
    );

    // Phase 3 runs boxed: the one-test-fn pattern builds one giant future,
    // and procurement pushed it past the test thread's stack.
    Box::pin(procurement_phase(
        db.clone(),
        numbering.clone(),
        &items,
        &moves,
        main.clone(),
        unit.clone(),
    ))
    .await;
}

/// Phase 3: procurement, end to end — suppliers, the purchase-to-pay walk,
/// three-way-match rejections, receipt reversal, FX costing and race test B.
async fn procurement_phase(
    db: DatabaseConnection,
    numbering: nebula::Numbering,
    items: &item::Store,
    moves: &MoveService,
    main: nebula_apps::scm::inventory::warehouse::Model,
    unit: item::uom::Model,
) {
    let suppliers = SupplierStore::new(db.clone());
    let orders = OrderService::new(db.clone());
    let receipts = ReceiptService::new(db.clone());
    let invoices = InvoiceService::new(db.clone());
    let queries = ProcurementQueries::new(db.clone());
    let gl = Gl::new(Events::new(), None);

    // --- supplier master ---
    let acme = suppliers
        .create(supplier_body("ACME", "Acme Supplies", "kes"), None)
        .await
        .unwrap();
    assert_eq!(acme.currency, "KES", "currency normalizes to upper case");
    assert!(
        matches!(
            suppliers.create(supplier_body("ACME", "Copycat", "KES"), None).await,
            Err(nebula::error::Error::Conflict(_))
        ),
        "supplier codes are unique"
    );

    // --- preferred supplier on the item master, now validated ---
    assert!(
        items
            .create_item(
                {
                    let mut body = item_body("PREF-1", "Preferring Widget", unit.id);
                    body.preferred_supplier_id = Some(Uuid::new_v4());
                    body
                },
                None,
            )
            .await
            .is_err(),
        "a preferred supplier must exist"
    );
    let p2p = items
        .create_item(
            {
                let mut body = item_body("P2P-1", "Procured Widget", unit.id);
                body.preferred_supplier_id = Some(acme.id);
                body
            },
            None,
        )
        .await
        .unwrap();

    // --- the catalog, maintained by hand ---
    let catalog_row = suppliers
        .upsert_catalog(
            acme.id,
            ItemSupplierBody {
                item_id: p2p.id,
                supplier_sku: Some("ACME-77".into()),
                supplier_item_name: None,
                purchase_uom_id: None,
                pack_qty: None,
                lead_time_days: Some(7),
                min_order_qty: None,
                is_preferred: true,
                is_active: true,
                notes: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(catalog_row.supplier_sku.as_deref(), Some("ACME-77"));

    // --- the full purchase-to-pay walk: PO 10 @ 50 ---
    let po = orders
        .create_draft(purchase_order(
            acme.id,
            main.id,
            vec![po_line(p2p.id, Decimal::from(10), Decimal::from(50))],
        ))
        .await
        .unwrap();
    assert_eq!(po.status, OrderStatus::Draft);
    assert!(po.number.is_none(), "drafts are unnumbered");
    assert_eq!(po.total, Decimal::from(500));

    // No receipts before the order is a real, approved commitment.
    assert!(
        receipts
            .create_draft(goods_receipt(
                po.id,
                vec![gr_line(po.lines[0].id, Decimal::from(1))],
            ))
            .await
            .is_err(),
        "a draft order cannot receive goods"
    );

    let po = orders.submit(po.id, &numbering, None).await.unwrap();
    assert_eq!(po.status, OrderStatus::Submitted);
    assert!(
        po.number.as_deref().unwrap().starts_with("PO-"),
        "the number lands at submit"
    );
    let po = orders.approve(po.id, None, None).await.unwrap();
    assert_eq!(po.status, OrderStatus::Approved);
    assert_eq!(
        on_order_of(&db, p2p.id, main.id).await,
        Decimal::from(10),
        "approval commits the demand"
    );
    let po_line_id = po.lines[0].id;

    // Receive 6 of 10.
    let rcpt6 = receipts
        .create_draft(goods_receipt(po.id, vec![gr_line(po_line_id, Decimal::from(6))]))
        .await
        .unwrap();
    let rcpt6 = receipts.post(rcpt6.id, &numbering, &gl).await.unwrap();
    assert_eq!(rcpt6.status, ReceiptStatus::Posted);
    assert!(rcpt6.number.as_deref().unwrap().starts_with("GRN-"));
    let po_view = orders.view(po.id).await.unwrap();
    assert_eq!(po_view.status, OrderStatus::PartiallyReceived);
    assert_eq!(po_view.lines[0].received_qty, Decimal::from(6));
    assert_eq!(
        level_of(&db, p2p.id, main.id).await,
        (Decimal::from(6), Decimal::from(300)),
        "stock arrives at the PO price"
    );
    assert_eq!(on_order_of(&db, p2p.id, main.id).await, Decimal::from(4));
    assert_eq!(
        queries.grni().await.unwrap().total,
        Decimal::from(300),
        "GRNI = received, not billed"
    );

    // The catalog remembered the price paid.
    let catalog = suppliers.catalog(acme.id).await.unwrap();
    let learned = catalog.iter().find(|c| c.item_id == p2p.id).unwrap();
    assert_eq!(learned.last_price, Some(Decimal::from(50)));

    // Over-receipt: only 4 remain.
    let over = receipts
        .create_draft(goods_receipt(po.id, vec![gr_line(po_line_id, Decimal::from(5))]))
        .await
        .unwrap();
    let err = receipts.post(over.id, &numbering, &gl).await.unwrap_err();
    assert!(
        err.to_string().contains("exceeds"),
        "over-receipt is rejected: {err}"
    );
    receipts.delete_draft(over.id).await.unwrap();

    // Receive the remaining 4.
    let rcpt4 = receipts
        .create_draft(goods_receipt(po.id, vec![gr_line(po_line_id, Decimal::from(4))]))
        .await
        .unwrap();
    receipts.post(rcpt4.id, &numbering, &gl).await.unwrap();
    let po_view = orders.view(po.id).await.unwrap();
    assert_eq!(po_view.status, OrderStatus::Received);
    assert_eq!(
        level_of(&db, p2p.id, main.id).await,
        (Decimal::from(10), Decimal::from(500))
    );
    assert_eq!(on_order_of(&db, p2p.id, main.id).await, Decimal::ZERO);
    assert_eq!(queries.grni().await.unwrap().total, Decimal::from(500));

    // A movement generated by procurement reverses only through it.
    let source_move = rcpt6.move_id.unwrap();
    let err = moves
        .reverse(source_move, "sneaky", None, &numbering, None, &gl)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("must be reversed through"),
        "source-generated movements are protected: {err}"
    );

    // Cancelling after posted receipts is off the table.
    assert!(
        orders.cancel(po.id, "changed my mind", None).await.is_err(),
        "an order with posted receipts cannot be cancelled"
    );

    // Three-way match: billing more than received.
    let overbill = invoices
        .create_draft(vendor_bill(
            acme.id,
            po.id,
            "INV-100",
            vec![bill_line(po_line_id, Decimal::from(20), Decimal::from(50))],
        ))
        .await
        .unwrap();
    let err = invoices.post(overbill.id, &numbering, None, &gl).await.unwrap_err();
    assert!(err.to_string().contains("exceeds"), "overbilling rejected: {err}");
    invoices.delete_draft(overbill.id).await.unwrap();

    // Three-way match: price disagreement.
    let priced_wrong = invoices
        .create_draft(vendor_bill(
            acme.id,
            po.id,
            "INV-101",
            vec![bill_line(po_line_id, Decimal::from(10), Decimal::from(55))],
        ))
        .await
        .unwrap();
    let err = invoices.post(priced_wrong.id, &numbering, None, &gl).await.unwrap_err();
    assert!(
        err.to_string().contains("does not match"),
        "price mismatch rejected: {err}"
    );
    invoices.delete_draft(priced_wrong.id).await.unwrap();

    // Bill the full ten at the agreed price.
    let bill = invoices
        .create_draft(vendor_bill(
            acme.id,
            po.id,
            "INV-001",
            vec![bill_line(po_line_id, Decimal::from(10), Decimal::from(50))],
        ))
        .await
        .unwrap();
    let bill = invoices.post(bill.id, &numbering, None, &gl).await.unwrap();
    assert_eq!(bill.status, InvoiceStatus::Posted);
    assert!(bill.number.as_deref().unwrap().starts_with("PINV-"));
    assert_eq!(bill.total, Decimal::from(500));
    assert_eq!(
        queries.grni().await.unwrap().total,
        Decimal::ZERO,
        "billing everything empties the GRNI"
    );
    let balances = queries.supplier_balances().await.unwrap();
    assert_eq!(balances.total_base, Decimal::from(500));

    // One supplier document, one entry.
    assert!(
        matches!(
            invoices
                .create_draft(vendor_bill(
                    acme.id,
                    po.id,
                    "INV-001",
                    vec![bill_line(po_line_id, Decimal::ONE, Decimal::from(50))],
                ))
                .await,
            Err(nebula::error::Error::Conflict(_))
        ),
        "duplicate supplier invoice number conflicts"
    );

    // Cancelling the bill reopens the GRNI; a corrected entry closes it.
    invoices.cancel(bill.id, "wrong period", None, &gl).await.unwrap();
    assert_eq!(queries.grni().await.unwrap().total, Decimal::from(500));
    let rebill = invoices
        .create_draft(vendor_bill(
            acme.id,
            po.id,
            "INV-002",
            vec![bill_line(po_line_id, Decimal::from(10), Decimal::from(50))],
        ))
        .await
        .unwrap();
    invoices.post(rebill.id, &numbering, None, &gl).await.unwrap();
    assert_eq!(queries.grni().await.unwrap().total, Decimal::ZERO);

    // A referenced supplier deactivates instead of deleting.
    let deleted = suppliers.delete(acme.id).await.unwrap();
    assert!(!deleted.is_active, "referenced suppliers deactivate");
    assert!(
        suppliers.find_by_id(acme.id).await.is_ok(),
        "the row is still there for history"
    );
    // Reactivate; the remaining sections keep buying from Acme.
    let mut reactivate = supplier_body("ACME", "Acme Supplies", "KES");
    reactivate.is_active = true;
    suppliers.update(acme.id, reactivate, None).await.unwrap();

    // --- receipt reversal restores the order and the stock ---
    let rev_item = items
        .create_item(item_body("REV-P1", "Returnable Widget", unit.id), None)
        .await
        .unwrap();
    let rev_po = orders
        .create_draft(purchase_order(
            acme.id,
            main.id,
            vec![po_line(rev_item.id, Decimal::from(5), Decimal::from(20))],
        ))
        .await
        .unwrap();
    let rev_po = orders.submit(rev_po.id, &numbering, None).await.unwrap();
    let rev_po = orders.approve(rev_po.id, None, None).await.unwrap();
    let rev_po_line = rev_po.lines[0].id;
    let rev_rcpt = receipts
        .create_draft(goods_receipt(rev_po.id, vec![gr_line(rev_po_line, Decimal::from(5))]))
        .await
        .unwrap();
    let rev_rcpt = receipts.post(rev_rcpt.id, &numbering, &gl).await.unwrap();
    assert_eq!(
        level_of(&db, rev_item.id, main.id).await,
        (Decimal::from(5), Decimal::from(100))
    );

    let reversal = receipts
        .reverse(rev_rcpt.id, "damaged on arrival", &numbering, None, &gl)
        .await
        .unwrap();
    assert_eq!(reversal.reverses_id, Some(rev_rcpt.id));
    assert!(reversal.number.as_deref().unwrap().starts_with("GRN-"));
    assert_eq!(
        level_of(&db, rev_item.id, main.id).await,
        (Decimal::ZERO, Decimal::ZERO),
        "the stock went back"
    );
    assert_eq!(
        on_order_of(&db, rev_item.id, main.id).await,
        Decimal::from(5),
        "the demand reopened"
    );
    let rev_po_view = orders.view(rev_po.id).await.unwrap();
    assert_eq!(rev_po_view.status, OrderStatus::Approved);
    assert_eq!(rev_po_view.lines[0].received_qty, Decimal::ZERO);
    let original = receipts.view(rev_rcpt.id).await.unwrap();
    assert_eq!(original.status, ReceiptStatus::Reversed);
    assert_eq!(original.reversed_by_id, Some(reversal.id));
    assert!(
        receipts.reverse(rev_rcpt.id, "again", &numbering, None, &gl).await.is_err(),
        "a receipt reverses once"
    );

    // Billed goods block the reversal until the invoice is cancelled.
    let rev_rcpt3 = receipts
        .create_draft(goods_receipt(rev_po.id, vec![gr_line(rev_po_line, Decimal::from(3))]))
        .await
        .unwrap();
    let rev_rcpt3 = receipts.post(rev_rcpt3.id, &numbering, &gl).await.unwrap();
    let rev_bill = invoices
        .create_draft(vendor_bill(
            acme.id,
            rev_po.id,
            "INV-200",
            vec![bill_line(rev_po_line, Decimal::from(3), Decimal::from(20))],
        ))
        .await
        .unwrap();
    invoices.post(rev_bill.id, &numbering, None, &gl).await.unwrap();
    let err = receipts
        .reverse(rev_rcpt3.id, "too late", &numbering, None, &gl)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("cancel the invoice"),
        "billed receipts cannot silently un-receive: {err}"
    );

    // --- a held supplier takes no new orders ---
    let mut held_body = supplier_body("HOLD-1", "Difficult Supplies", "KES");
    held_body.on_hold = true;
    held_body.hold_reason = Some("quality dispute".into());
    let held = suppliers.create(held_body, None).await.unwrap();
    let err = orders
        .create_draft(purchase_order(
            held.id,
            main.id,
            vec![po_line(p2p.id, Decimal::ONE, Decimal::ONE)],
        ))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("on hold"),
        "held suppliers take no new orders: {err}"
    );

    // --- cancelling an approved order releases its demand ---
    let cxl_po = orders
        .create_draft(purchase_order(
            acme.id,
            main.id,
            vec![po_line(rev_item.id, Decimal::from(3), Decimal::from(7))],
        ))
        .await
        .unwrap();
    let cxl_po = orders.submit(cxl_po.id, &numbering, None).await.unwrap();
    orders.approve(cxl_po.id, None, None).await.unwrap();
    let committed = on_order_of(&db, rev_item.id, main.id).await;
    let cxl_po = orders.cancel(cxl_po.id, "budget cut", None).await.unwrap();
    assert_eq!(cxl_po.status, OrderStatus::Cancelled);
    assert_eq!(
        on_order_of(&db, rev_item.id, main.id).await,
        committed - Decimal::from(3),
        "cancellation releases exactly its own demand"
    );

    // --- an FX order values stock at price x rate, exactly ---
    let fx_supplier = suppliers
        .create(supplier_body("FX-1", "Overseas Inc", "USD"), None)
        .await
        .unwrap();
    let fx_item = items
        .create_item(item_body("FX-P1", "Imported Widget", unit.id), None)
        .await
        .unwrap();
    let fx_po = orders
        .create_draft(purchase_order(
            fx_supplier.id,
            main.id,
            vec![po_line(fx_item.id, Decimal::from(4), Decimal::new(25, 1))],
        ))
        .await
        .unwrap();
    assert_eq!(fx_po.currency, "USD", "orders default to the supplier currency");
    let fx_po = orders.submit(fx_po.id, &numbering, None).await.unwrap();
    let fx_po = orders
        .approve(fx_po.id, Some(Decimal::from(4)), None)
        .await
        .unwrap();
    assert_eq!(fx_po.exchange_rate, Decimal::from(4));
    let fx_rcpt = receipts
        .create_draft(goods_receipt(
            fx_po.id,
            vec![gr_line(fx_po.lines[0].id, Decimal::from(4))],
        ))
        .await
        .unwrap();
    receipts.post(fx_rcpt.id, &numbering, &gl).await.unwrap();
    assert_eq!(
        level_of(&db, fx_item.id, main.id).await,
        (Decimal::from(4), Decimal::from(40)),
        "2.5 USD x rate 4 = 10 base per unit, exactly"
    );

    // --- race test B: two receipts racing the last 4 on a PO line ---
    // Both lock their own receipt row, then queue on the order row; the
    // loser revalidates against the winner's received_qty and fails the
    // remaining-balance check.
    let race_item = items
        .create_item(item_body("RACE-P1", "Contended Procured Widget", unit.id), None)
        .await
        .unwrap();
    let race_po = orders
        .create_draft(purchase_order(
            acme.id,
            main.id,
            vec![po_line(race_item.id, Decimal::from(10), Decimal::ONE)],
        ))
        .await
        .unwrap();
    let race_po = orders.submit(race_po.id, &numbering, None).await.unwrap();
    let race_po = orders.approve(race_po.id, None, None).await.unwrap();
    let race_po_line = race_po.lines[0].id;
    let first6 = receipts
        .create_draft(goods_receipt(race_po.id, vec![gr_line(race_po_line, Decimal::from(6))]))
        .await
        .unwrap();
    receipts.post(first6.id, &numbering, &gl).await.unwrap();

    let mut race_receipts = Vec::new();
    for _ in 0..2 {
        let draft = receipts
            .create_draft(goods_receipt(
                race_po.id,
                vec![gr_line(race_po_line, Decimal::from(4))],
            ))
            .await
            .unwrap();
        race_receipts.push(draft.id);
    }
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for receipt_id in race_receipts {
        let db = db.clone();
        let numbering = numbering.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let gl = Gl::new(Events::new(), None);
            barrier.wait().await;
            ReceiptService::new(db).post(receipt_id, &numbering, &gl).await
        }));
    }
    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.expect("race task must not panic"));
    }
    let wins = outcomes.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "exactly one racing receipt of the last 4 succeeds");
    let loss = outcomes.iter().find(|r| r.is_err()).unwrap();
    let loss_msg = loss.as_ref().unwrap_err().to_string();
    // The winner completes the order, so the loser trips the lifecycle
    // guard ("is received and cannot receive goods"); had the line not
    // completed, it would trip the remaining-balance check instead. Either
    // way it must be a validation, never a deadlock.
    assert!(
        loss_msg.contains("cannot receive goods") || loss_msg.contains("exceeds"),
        "the loser fails validation, not a deadlock: {loss_msg}"
    );
    let race_po_view = orders.view(race_po.id).await.unwrap();
    assert_eq!(race_po_view.status, OrderStatus::Received);
    assert_eq!(
        race_po_view.lines[0].received_qty,
        Decimal::from(10),
        "the counters never overshoot"
    );
    assert_eq!(
        level_of(&db, race_item.id, main.id).await,
        (Decimal::from(10), Decimal::from(10))
    );
}
