//! The supply-chain app: stock control, the purchase-to-pay cycle and
//! the order-to-cash cycle.
//!
//! One app, three submodules — the way accounting contains its journal
//! and chart. Posting a goods receipt must update the purchase order
//! *and* write stock in a single database transaction (and a sales
//! delivery the same, on the way out), and the "apps never import each
//! other" rule makes that impossible across separate apps, so they live
//! together:
//!
//! - [`inventory`] — items, warehouses, the immutable stock ledger with
//!   its per-item×warehouse level cache, movement documents
//!   (receipt/issue/transfer/adjustment) and moving-average valuation
//! - [`procurement`] — suppliers, purchase orders with approval, goods
//!   receipts that drive the stock engine, purchase invoices with the
//!   three-way match, and the GRNI view
//! - [`sales`] — customers, price lists with a provenance-recording
//!   resolution chain, quotations, sales orders with reservation and
//!   credit control; deliveries, invoices, credit notes and customer
//!   payments arrive in their own phases
//!
//! Table prefixes stay per submodule (`inventory_*`, `procurement_*`) so a
//! future split into separate crates never needs renames. Every tenant is
//! seeded with a default warehouse and starter units of measure so stock
//! can move with zero configuration. Inventory is **perpetual**: every
//! posted document with a financial effect publishes a posting request
//! over the framework's GL port ([`gl`]), which the accounting app books
//! against role-resolved seeded accounts — see the posting matrix in
//! [`gl`]'s module docs.
//!
//! Depends on [`AdministrationModule`]: stock is moved by signed-in
//! people of a tenant.

pub mod document;
pub mod gl;
pub mod inventory;
pub mod procurement;
pub mod sales;
pub mod seed;

use nebula::error::Result;
use nebula::tenancy::{TenantCreated, TenantManager};
use nebula::{AdministrationModule, Module, ModuleContext, Reset, SeriesDef};
use std::sync::Arc;
use uuid::Uuid;

/// Number series for goods receipts into stock. Shared deliberately:
/// a procurement goods receipt allocates from this same series and stamps
/// the one GRN number on both its own row and the stock movement it
/// creates, so there is a single GRN sequence whether goods arrive against
/// a purchase order or are received directly.
pub(crate) const RECEIPT_SERIES: &str = "inventory.receipt";

/// Number series for stock issues.
pub(crate) const ISSUE_SERIES: &str = "inventory.issue";

/// Number series for stock transfers between warehouses.
pub(crate) const TRANSFER_SERIES: &str = "inventory.transfer";

/// Number series for stock adjustments (counts / opening stock).
pub(crate) const ADJUSTMENT_SERIES: &str = "inventory.adjustment";

/// Number series for purchase orders, allocated at submit.
pub(crate) const ORDER_SERIES: &str = "procurement.order";

/// Number series for purchase invoices (vendor bills).
pub(crate) const INVOICE_SERIES: &str = "procurement.invoice";

/// Number series for supplier payments, allocated at post.
pub(crate) const PAYMENT_SERIES: &str = "procurement.payment";

/// Number series for purchase returns (return to supplier). Like goods
/// receipts, the one RTS number lands on the return and on the stock
/// movement it creates.
pub(crate) const RETURN_SERIES: &str = "procurement.return";

/// Number series for purchase requisitions, allocated at submit.
pub(crate) const REQUISITION_SERIES: &str = "procurement.requisition";

/// Number series for requests for quotation, allocated at send.
pub(crate) const RFQ_SERIES: &str = "procurement.rfq";

/// Number series for sales quotations, allocated at send.
pub(crate) const SALES_QUOTATION_SERIES: &str = "sales.quotation";

/// Number series for sales orders, allocated at confirmation.
pub(crate) const SALES_ORDER_SERIES: &str = "sales.order";

/// Number series for delivery notes. Like goods receipts and purchase
/// returns, the one DN number lands on the delivery and on the stock
/// movement it creates.
pub(crate) const SALES_DELIVERY_SERIES: &str = "sales.delivery";

/// Number series for sales invoices, allocated at post.
pub(crate) const SALES_INVOICE_SERIES: &str = "sales.invoice";

/// Number series for credit notes, allocated at post.
pub(crate) const SALES_CREDIT_NOTE_SERIES: &str = "sales.credit_note";

/// Number series for customer payments (receipts), allocated at post.
pub(crate) const SALES_PAYMENT_SERIES: &str = "sales.payment";

/// The currency the walk-in customer is seeded in when the tenant has
/// none configured (the accounting app's fallback, for the same reason).
const FALLBACK_CURRENCY: &str = "USD";

pub struct ScmApp;

impl Module for ScmApp {
    fn name(&self) -> &'static str {
        "scm"
    }

    fn depends_on(&self) -> Vec<Box<dyn Module>> {
        vec![Box::new(AdministrationModule)]
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.add_permissions(inventory::permissions::tree());
        ctx.add_permissions(procurement::permissions::tree());
        ctx.add_permissions(sales::permissions::tree());

        for (key, name, template) in [
            (RECEIPT_SERIES, "Goods Receipt", "GRN-{YYYY}-{SEQ:5}"),
            (ISSUE_SERIES, "Stock Issue", "ISS-{YYYY}-{SEQ:5}"),
            (TRANSFER_SERIES, "Stock Transfer", "TRF-{YYYY}-{SEQ:5}"),
            (ADJUSTMENT_SERIES, "Stock Adjustment", "ADJ-{YYYY}-{SEQ:5}"),
            (ORDER_SERIES, "Purchase Order", "PO-{YYYY}-{SEQ:5}"),
            (INVOICE_SERIES, "Purchase Invoice", "PINV-{YYYY}-{SEQ:5}"),
            (PAYMENT_SERIES, "Supplier Payment", "PAY-{YYYY}-{SEQ:5}"),
            (RETURN_SERIES, "Purchase Return", "RTS-{YYYY}-{SEQ:5}"),
            (REQUISITION_SERIES, "Purchase Requisition", "REQ-{YYYY}-{SEQ:5}"),
            (RFQ_SERIES, "Request for Quotation", "RFQ-{YYYY}-{SEQ:5}"),
            (SALES_QUOTATION_SERIES, "Quotation", "QUO-{YYYY}-{SEQ:5}"),
            (SALES_ORDER_SERIES, "Sales Order", "SO-{YYYY}-{SEQ:5}"),
            (SALES_DELIVERY_SERIES, "Delivery Note", "DN-{YYYY}-{SEQ:5}"),
            (SALES_INVOICE_SERIES, "Sales Invoice", "SINV-{YYYY}-{SEQ:5}"),
            (SALES_CREDIT_NOTE_SERIES, "Credit Note", "CN-{YYYY}-{SEQ:5}"),
            (SALES_PAYMENT_SERIES, "Customer Payment", "RCT-{YYYY}-{SEQ:5}"),
        ] {
            ctx.declare_series(
                SeriesDef::new(key, name, template, Reset::Yearly)
                    .expect("valid scm series template"),
            );
        }

        ctx.add_api(inventory::item::api());
        ctx.add_api(inventory::warehouse::api());
        ctx.add_api(inventory::moves::api());
        ctx.add_api(inventory::levels::api());
        ctx.add_api(inventory::batch::api());
        ctx.add_api(procurement::supplier::api());
        ctx.add_api(procurement::requisition::api());
        ctx.add_api(procurement::rfq::api());
        ctx.add_api(procurement::order::api());
        ctx.add_api(procurement::receipt::api());
        ctx.add_api(procurement::returns::api());
        ctx.add_api(procurement::invoice::api());
        ctx.add_api(procurement::payment::api());
        ctx.add_api(procurement::reorder::api());
        ctx.add_api(procurement::reports::api());
        ctx.add_api(sales::customer::api());
        ctx.add_api(sales::pricing::api());
        ctx.add_api(sales::quotation::api());
        ctx.add_api(sales::order::api());
        ctx.add_api(sales::delivery::api());
        ctx.add_api(sales::invoice::api());
        ctx.add_api(sales::credit_note::api());
        ctx.add_api(sales::payment::api());
        ctx.add_api(sales::reports::api());
        ctx.add_api(gl::api());
        ctx.add_routes(
            inventory::item::routes()
                .merge(inventory::warehouse::routes())
                .merge(inventory::moves::routes())
                .merge(inventory::levels::routes())
                .merge(inventory::batch::routes())
                .merge(procurement::supplier::routes())
                .merge(procurement::requisition::routes())
                .merge(procurement::rfq::routes())
                .merge(procurement::order::routes())
                .merge(procurement::receipt::routes())
                .merge(procurement::returns::routes())
                .merge(procurement::invoice::routes())
                .merge(procurement::payment::routes())
                .merge(procurement::reorder::routes())
                .merge(procurement::reports::routes())
                .merge(sales::customer::routes())
                .merge(sales::pricing::routes())
                .merge(sales::quotation::routes())
                .merge(sales::order::routes())
                .merge(sales::delivery::routes())
                .merge(sales::invoice::routes())
                .merge(sales::credit_note::routes())
                .merge(sales::payment::routes())
                .merge(sales::reports::routes())
                .merge(gl::routes()),
        );

        ctx.declare_report(Arc::new(inventory::reports::StockBalanceReport));
        ctx.declare_report(Arc::new(inventory::reports::StockLedgerReport));
        ctx.declare_report(Arc::new(inventory::reports::ValuationSummaryReport));
        ctx.declare_report(Arc::new(inventory::reports::ReorderReport));
        ctx.declare_report(Arc::new(procurement::documents::RequisitionDocument));
        ctx.declare_report(Arc::new(procurement::documents::PurchaseOrderDocument));
        ctx.declare_report(Arc::new(procurement::documents::GoodsReceiptDocument));
        ctx.declare_report(Arc::new(procurement::documents::SupplierInvoiceDocument));
        ctx.declare_report(Arc::new(procurement::documents::PurchaseReturnDocument));
        ctx.declare_report(Arc::new(procurement::reports::GrniReport));
        ctx.declare_report(Arc::new(procurement::reports::SupplierBalancesReport));
        ctx.declare_report(Arc::new(procurement::reports::SupplierScorecardReport));
        ctx.declare_report(Arc::new(sales::documents::QuotationDocument));
        ctx.declare_report(Arc::new(sales::documents::SalesOrderDocument));
        ctx.declare_report(Arc::new(sales::documents::DeliveryNoteDocument));
        ctx.declare_report(Arc::new(sales::documents::SalesInvoiceDocument));
        ctx.declare_report(Arc::new(sales::documents::CreditNoteDocument));
        ctx.declare_report(Arc::new(sales::reports::ArAgingReport));
        ctx.declare_report(Arc::new(sales::reports::DeliveredNotBilledReport));
        ctx.declare_report(Arc::new(sales::reports::SalesRegisterReport));
        ctx.declare_report(Arc::new(sales::reports::SalesMarginsReport));
        ctx.declare_report(Arc::new(sales::reports::ArReconciliationReport));
        ctx.declare_report(Arc::new(gl::GlReconciliationReport));

        // GL integration: clear outbox rows on accounting's acknowledgement
        // and re-publish anything that lingers unbooked.
        gl::subscribe_acks(ctx);
        gl::spawn_sweeper(ctx);

        // Auto reorder: draft purchase orders for short stock positions.
        procurement::reorder::spawn_worker(ctx);

        self.seed_tenants(ctx);
    }
}

impl ScmApp {
    /// Seed every tenant with the default warehouse and starter UoMs: new
    /// tenants react to [`TenantCreated`], existing tenants are rolled out
    /// once at boot (both idempotent). Migrations have already run for
    /// every database by the time `configure` is called.
    fn seed_tenants(&self, ctx: &mut ModuleContext) {
        match ctx.tenants() {
            Some(tenants) => {
                let on_create = tenants.clone();
                ctx.events().subscribe::<TenantCreated, _, _>(move |ev| {
                    let tenants = on_create.clone();
                    async move { seed_for_tenant(&tenants, ev.tenant_id).await }
                });

                let rollout = tenants.clone();
                tokio::spawn(async move {
                    match rollout.find_all().await {
                        Ok(list) => {
                            for tenant in list.into_iter().filter(|t| t.is_active) {
                                if let Err(e) = seed_for_tenant(&rollout, tenant.id).await {
                                    tracing::warn!(tenant = %tenant.name, error = %e,
                                        "scm seed rollout failed");
                                }
                            }
                        }
                        Err(e) => tracing::warn!(error = %e,
                            "could not list tenants for the scm seed rollout"),
                    }
                });
            }
            // Single-tenant: the app runs against the main database.
            None => {
                if let Some(db) = ctx.db() {
                    let db = db.clone();
                    tokio::spawn(async move {
                        if let Err(e) = seed::seed_defaults(&db, FALLBACK_CURRENCY).await {
                            tracing::warn!(error = %e, "scm seed failed");
                        }
                    });
                }
            }
        }
    }
}

/// Seed one tenant's warehouse, UoM and walk-in-customer defaults
/// (idempotent), the customer in the tenant's own currency.
async fn seed_for_tenant(tenants: &TenantManager, tenant_id: Uuid) -> Result<()> {
    let Some(tenant) = tenants.find_by_id(tenant_id).await? else {
        return Ok(());
    };
    let db = tenants.connection_for(&tenant).await?;
    let currency = tenant
        .default_currency
        .as_deref()
        .unwrap_or(FALLBACK_CURRENCY);
    if seed::seed_defaults(&db, currency).await? {
        tracing::info!(tenant = %tenant.name,
            "seeded default warehouse, units of measure and walk-in customer");
    }
    Ok(())
}
