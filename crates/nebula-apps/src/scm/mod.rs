//! The supply-chain app: stock control and the purchase-to-pay cycle.
//!
//! One app, two submodules — the way accounting contains its journal and
//! chart. Posting a goods receipt must update the purchase order *and*
//! write stock in a single database transaction, and the "apps never
//! import each other" rule makes that impossible across two apps, so
//! inventory and procurement live together:
//!
//! - [`inventory`] — items, warehouses, the immutable stock ledger with
//!   its per-item×warehouse level cache, movement documents
//!   (receipt/issue/transfer/adjustment) and moving-average valuation
//! - [`procurement`] — suppliers, purchase orders with approval, goods
//!   receipts that drive the stock engine, purchase invoices with the
//!   three-way match, and the GRNI view
//!
//! Table prefixes stay per submodule (`inventory_*`, `procurement_*`) so a
//! future split into separate crates never needs renames. Every tenant is
//! seeded with a default warehouse and starter units of measure so stock
//! can move with zero configuration. GL integration is a later phase:
//! documents carry the seams (account-role columns, movement `source`)
//! but book nothing yet — inventory is periodic from the GL's point of
//! view.
//!
//! Depends on [`AdministrationModule`]: stock is moved by signed-in
//! people of a tenant.

pub mod inventory;
pub mod procurement;
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

        for (key, name, template) in [
            (RECEIPT_SERIES, "Goods Receipt", "GRN-{YYYY}-{SEQ:5}"),
            (ISSUE_SERIES, "Stock Issue", "ISS-{YYYY}-{SEQ:5}"),
            (TRANSFER_SERIES, "Stock Transfer", "TRF-{YYYY}-{SEQ:5}"),
            (ADJUSTMENT_SERIES, "Stock Adjustment", "ADJ-{YYYY}-{SEQ:5}"),
            (ORDER_SERIES, "Purchase Order", "PO-{YYYY}-{SEQ:5}"),
            (INVOICE_SERIES, "Purchase Invoice", "PINV-{YYYY}-{SEQ:5}"),
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
        ctx.add_api(procurement::supplier::api());
        ctx.add_api(procurement::order::api());
        ctx.add_api(procurement::receipt::api());
        ctx.add_api(procurement::invoice::api());
        ctx.add_api(procurement::reports::api());
        ctx.add_routes(
            inventory::item::routes()
                .merge(inventory::warehouse::routes())
                .merge(inventory::moves::routes())
                .merge(inventory::levels::routes())
                .merge(procurement::supplier::routes())
                .merge(procurement::order::routes())
                .merge(procurement::receipt::routes())
                .merge(procurement::invoice::routes())
                .merge(procurement::reports::routes()),
        );

        ctx.declare_report(Arc::new(inventory::reports::StockBalanceReport));
        ctx.declare_report(Arc::new(inventory::reports::StockLedgerReport));
        ctx.declare_report(Arc::new(inventory::reports::ValuationSummaryReport));
        ctx.declare_report(Arc::new(inventory::reports::ReorderReport));
        ctx.declare_report(Arc::new(procurement::reports::GrniReport));
        ctx.declare_report(Arc::new(procurement::reports::SupplierBalancesReport));

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
                        if let Err(e) = seed::seed_defaults(&db).await {
                            tracing::warn!(error = %e, "scm seed failed");
                        }
                    });
                }
            }
        }
    }
}

/// Seed one tenant's warehouse and UoM defaults (idempotent).
async fn seed_for_tenant(tenants: &TenantManager, tenant_id: Uuid) -> Result<()> {
    let Some(tenant) = tenants.find_by_id(tenant_id).await? else {
        return Ok(());
    };
    let db = tenants.connection_for(&tenant).await?;
    if seed::seed_defaults(&db).await? {
        tracing::info!(tenant = %tenant.name, "seeded default warehouse and units of measure");
    }
    Ok(())
}
