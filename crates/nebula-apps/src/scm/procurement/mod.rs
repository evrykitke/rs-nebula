//! The purchase-to-pay cycle: suppliers, purchase orders, goods receipts,
//! purchase invoices.
//!
//! The purchase order is the hub. Receipts and invoices both reference PO
//! lines and maintain the cumulative `received_qty` / `billed_qty` pair —
//! that pair is what makes partial deliveries, partial billing, the
//! over-receipt guard and the GRNI (goods received, not invoiced) report
//! all fall out naturally. Posting a goods receipt writes the stock
//! movement through [`super::inventory`]'s engine in the same database
//! transaction; posting an invoice runs the three-way match (authorized
//! against the PO, quantity within what was received, price matching the
//! order) — the classic control against paying for goods never received.
//!
//! Approval is permission-based: submitting is `Orders.Submit`, approving
//! is `Orders.Approve`; amount thresholds and workflow come later, as do
//! payments (accounting's domain).

pub mod invoice;
pub mod order;
pub mod receipt;
pub mod reorder;
pub mod reports;
pub mod requisition;
pub mod returns;
pub mod rfq;
pub mod supplier;

pub mod permissions {
    use nebula::auth::PermissionDef;

    pub mod names {
        pub const PROCUREMENT: &str = "Pages.Procurement";
        pub const SUPPLIERS: &str = "Pages.Procurement.Suppliers";
        pub const SUPPLIERS_VIEW: &str = "Pages.Procurement.Suppliers.View";
        pub const SUPPLIERS_CREATE: &str = "Pages.Procurement.Suppliers.Create";
        pub const SUPPLIERS_EDIT: &str = "Pages.Procurement.Suppliers.Edit";
        pub const SUPPLIERS_DELETE: &str = "Pages.Procurement.Suppliers.Delete";
        pub const REQUISITIONS: &str = "Pages.Procurement.Requisitions";
        pub const REQUISITIONS_VIEW: &str = "Pages.Procurement.Requisitions.View";
        pub const REQUISITIONS_CREATE: &str = "Pages.Procurement.Requisitions.Create";
        pub const REQUISITIONS_SUBMIT: &str = "Pages.Procurement.Requisitions.Submit";
        pub const REQUISITIONS_APPROVE: &str = "Pages.Procurement.Requisitions.Approve";
        pub const REQUISITIONS_CONVERT: &str = "Pages.Procurement.Requisitions.Convert";
        pub const RFQS: &str = "Pages.Procurement.Rfqs";
        pub const RFQS_VIEW: &str = "Pages.Procurement.Rfqs.View";
        pub const RFQS_CREATE: &str = "Pages.Procurement.Rfqs.Create";
        pub const RFQS_SEND: &str = "Pages.Procurement.Rfqs.Send";
        pub const RFQS_RECORD_QUOTES: &str = "Pages.Procurement.Rfqs.RecordQuotes";
        pub const RFQS_AWARD: &str = "Pages.Procurement.Rfqs.Award";
        pub const ORDERS: &str = "Pages.Procurement.Orders";
        pub const ORDERS_VIEW: &str = "Pages.Procurement.Orders.View";
        pub const ORDERS_CREATE: &str = "Pages.Procurement.Orders.Create";
        pub const ORDERS_SUBMIT: &str = "Pages.Procurement.Orders.Submit";
        pub const ORDERS_APPROVE: &str = "Pages.Procurement.Orders.Approve";
        pub const ORDERS_CANCEL: &str = "Pages.Procurement.Orders.Cancel";
        pub const RECEIPTS: &str = "Pages.Procurement.Receipts";
        pub const RECEIPTS_VIEW: &str = "Pages.Procurement.Receipts.View";
        pub const RECEIPTS_CREATE: &str = "Pages.Procurement.Receipts.Create";
        pub const RECEIPTS_POST: &str = "Pages.Procurement.Receipts.Post";
        pub const RECEIPTS_REVERSE: &str = "Pages.Procurement.Receipts.Reverse";
        pub const RETURNS: &str = "Pages.Procurement.Returns";
        pub const RETURNS_VIEW: &str = "Pages.Procurement.Returns.View";
        pub const RETURNS_CREATE: &str = "Pages.Procurement.Returns.Create";
        pub const RETURNS_POST: &str = "Pages.Procurement.Returns.Post";
        pub const RETURNS_REVERSE: &str = "Pages.Procurement.Returns.Reverse";
        pub const INVOICES: &str = "Pages.Procurement.Invoices";
        pub const INVOICES_VIEW: &str = "Pages.Procurement.Invoices.View";
        pub const INVOICES_CREATE: &str = "Pages.Procurement.Invoices.Create";
        pub const INVOICES_POST: &str = "Pages.Procurement.Invoices.Post";
        pub const INVOICES_CANCEL: &str = "Pages.Procurement.Invoices.Cancel";
        pub const REPORTS: &str = "Pages.Procurement.Reports";
        pub const REPORTS_VIEW: &str = "Pages.Procurement.Reports.View";
    }

    pub fn tree() -> PermissionDef {
        use names::*;
        PermissionDef::new(PROCUREMENT, "Procurement")
            .child(
                PermissionDef::new(SUPPLIERS, "Suppliers")
                    .child(PermissionDef::new(SUPPLIERS_VIEW, "View suppliers"))
                    .child(PermissionDef::new(SUPPLIERS_CREATE, "Create suppliers"))
                    .child(PermissionDef::new(SUPPLIERS_EDIT, "Edit suppliers"))
                    .child(PermissionDef::new(SUPPLIERS_DELETE, "Delete suppliers")),
            )
            .child(
                PermissionDef::new(REQUISITIONS, "Purchase requisitions")
                    .child(PermissionDef::new(REQUISITIONS_VIEW, "View requisitions"))
                    .child(PermissionDef::new(REQUISITIONS_CREATE, "Create requisitions"))
                    .child(PermissionDef::new(REQUISITIONS_SUBMIT, "Submit requisitions"))
                    .child(PermissionDef::new(
                        REQUISITIONS_APPROVE,
                        "Approve requisitions",
                    ))
                    .child(PermissionDef::new(
                        REQUISITIONS_CONVERT,
                        "Convert requisitions to orders",
                    )),
            )
            .child(
                PermissionDef::new(RFQS, "Requests for quotation")
                    .child(PermissionDef::new(RFQS_VIEW, "View RFQs"))
                    .child(PermissionDef::new(RFQS_CREATE, "Create RFQs"))
                    .child(PermissionDef::new(RFQS_SEND, "Send RFQs"))
                    .child(PermissionDef::new(RFQS_RECORD_QUOTES, "Record RFQ quotes"))
                    .child(PermissionDef::new(RFQS_AWARD, "Award RFQs")),
            )
            .child(
                PermissionDef::new(ORDERS, "Purchase orders")
                    .child(PermissionDef::new(ORDERS_VIEW, "View purchase orders"))
                    .child(PermissionDef::new(ORDERS_CREATE, "Create purchase orders"))
                    .child(PermissionDef::new(ORDERS_SUBMIT, "Submit purchase orders"))
                    .child(PermissionDef::new(ORDERS_APPROVE, "Approve purchase orders"))
                    .child(PermissionDef::new(ORDERS_CANCEL, "Cancel purchase orders")),
            )
            .child(
                PermissionDef::new(RECEIPTS, "Goods receipts")
                    .child(PermissionDef::new(RECEIPTS_VIEW, "View goods receipts"))
                    .child(PermissionDef::new(RECEIPTS_CREATE, "Create goods receipts"))
                    .child(PermissionDef::new(RECEIPTS_POST, "Post goods receipts"))
                    .child(PermissionDef::new(RECEIPTS_REVERSE, "Reverse goods receipts")),
            )
            .child(
                PermissionDef::new(RETURNS, "Purchase returns")
                    .child(PermissionDef::new(RETURNS_VIEW, "View purchase returns"))
                    .child(PermissionDef::new(RETURNS_CREATE, "Create purchase returns"))
                    .child(PermissionDef::new(RETURNS_POST, "Post purchase returns"))
                    .child(PermissionDef::new(
                        RETURNS_REVERSE,
                        "Reverse purchase returns",
                    )),
            )
            .child(
                PermissionDef::new(INVOICES, "Purchase invoices")
                    .child(PermissionDef::new(INVOICES_VIEW, "View purchase invoices"))
                    .child(PermissionDef::new(INVOICES_CREATE, "Create purchase invoices"))
                    .child(PermissionDef::new(INVOICES_POST, "Post purchase invoices"))
                    .child(PermissionDef::new(
                        INVOICES_CANCEL,
                        "Cancel purchase invoices",
                    )),
            )
            .child(
                PermissionDef::new(REPORTS, "Procurement reports")
                    .child(PermissionDef::new(REPORTS_VIEW, "View procurement reports")),
            )
    }
}
