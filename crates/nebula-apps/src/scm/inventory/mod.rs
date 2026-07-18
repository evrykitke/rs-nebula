//! Stock control: what the business holds, where, and at what value.
//!
//! The design is the accounting split applied to goods: an immutable
//! ledger (`inventory_stock_ledger`, one row per posted movement line,
//! never updated or deleted) and a mutable aggregate
//! (`inventory_stock_levels`, one row per item × warehouse, maintained in
//! the same transaction under a row lock). The ledger answers "how did we
//! get here", the level answers "what is on hand right now" — and doubles
//! as the concurrency gate: negative-stock checks run with the level row
//! held FOR UPDATE, so concurrent movements of the same stock serialize.
//!
//! Valuation is moving (weighted) average per item × warehouse in the
//! tenant base currency; valuation order is posting order, and a
//! backdated `entry_date` is descriptive only. Corrections are new
//! documents (a reversal or an adjustment), never edits.

pub mod batch;
pub mod item;
pub mod levels;
pub mod moves;
pub mod reports;
pub mod stock;
pub mod warehouse;
pub mod widgets;

pub mod permissions {
    use nebula::auth::PermissionDef;

    pub mod names {
        pub const INVENTORY: &str = "Pages.Inventory";
        pub const ITEMS: &str = "Pages.Inventory.Items";
        pub const ITEMS_VIEW: &str = "Pages.Inventory.Items.View";
        pub const ITEMS_CREATE: &str = "Pages.Inventory.Items.Create";
        pub const ITEMS_EDIT: &str = "Pages.Inventory.Items.Edit";
        pub const ITEMS_DELETE: &str = "Pages.Inventory.Items.Delete";
        pub const WAREHOUSES: &str = "Pages.Inventory.Warehouses";
        pub const WAREHOUSES_VIEW: &str = "Pages.Inventory.Warehouses.View";
        pub const WAREHOUSES_MANAGE: &str = "Pages.Inventory.Warehouses.Manage";
        pub const MOVEMENTS: &str = "Pages.Inventory.Movements";
        pub const MOVEMENTS_VIEW: &str = "Pages.Inventory.Movements.View";
        pub const MOVEMENTS_CREATE: &str = "Pages.Inventory.Movements.Create";
        pub const MOVEMENTS_POST: &str = "Pages.Inventory.Movements.Post";
        pub const MOVEMENTS_REVERSE: &str = "Pages.Inventory.Movements.Reverse";
        pub const ADJUSTMENTS: &str = "Pages.Inventory.Adjustments";
        pub const ADJUSTMENTS_VIEW: &str = "Pages.Inventory.Adjustments.View";
        pub const ADJUSTMENTS_CREATE: &str = "Pages.Inventory.Adjustments.Create";
        pub const ADJUSTMENTS_POST: &str = "Pages.Inventory.Adjustments.Post";
        pub const REPORTS: &str = "Pages.Inventory.Reports";
        pub const REPORTS_VIEW: &str = "Pages.Inventory.Reports.View";
    }

    pub fn tree() -> PermissionDef {
        use names::*;
        PermissionDef::new(INVENTORY, "Inventory")
            .child(
                PermissionDef::new(ITEMS, "Items")
                    .child(PermissionDef::new(ITEMS_VIEW, "View items"))
                    .child(PermissionDef::new(ITEMS_CREATE, "Create items"))
                    .child(PermissionDef::new(ITEMS_EDIT, "Edit items"))
                    .child(PermissionDef::new(ITEMS_DELETE, "Delete items")),
            )
            .child(
                PermissionDef::new(WAREHOUSES, "Warehouses")
                    .child(PermissionDef::new(WAREHOUSES_VIEW, "View warehouses"))
                    .child(PermissionDef::new(WAREHOUSES_MANAGE, "Manage warehouses")),
            )
            .child(
                PermissionDef::new(MOVEMENTS, "Stock movements")
                    .child(PermissionDef::new(MOVEMENTS_VIEW, "View stock movements"))
                    .child(PermissionDef::new(
                        MOVEMENTS_CREATE,
                        "Create stock movements",
                    ))
                    .child(PermissionDef::new(MOVEMENTS_POST, "Post stock movements"))
                    .child(PermissionDef::new(
                        MOVEMENTS_REVERSE,
                        "Reverse stock movements",
                    )),
            )
            .child(
                PermissionDef::new(ADJUSTMENTS, "Stock adjustments")
                    .child(PermissionDef::new(
                        ADJUSTMENTS_VIEW,
                        "View stock adjustments",
                    ))
                    .child(PermissionDef::new(
                        ADJUSTMENTS_CREATE,
                        "Create stock adjustments",
                    ))
                    .child(PermissionDef::new(
                        ADJUSTMENTS_POST,
                        "Post stock adjustments",
                    )),
            )
            .child(
                PermissionDef::new(REPORTS, "Inventory reports")
                    .child(PermissionDef::new(REPORTS_VIEW, "View inventory reports")),
            )
    }
}
