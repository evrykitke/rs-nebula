//! The order-to-cash cycle: customers, pricing, quotations, sales orders
//! with reservation and credit control, deliveries that drive the stock
//! engine, sales invoices, credit notes and customer payments.
//!
//! The sales order is the hub, exactly as the purchase order is in
//! [`super::procurement`]. Deliveries and invoices reference order lines
//! and maintain the cumulative `delivered_qty` / `billed_qty` pair — the
//! mirror of procurement's `received_qty` / `billed_qty` — which makes
//! partial deliveries, partial billing, the over-delivery guard and the
//! delivered-not-billed report all fall out naturally. Posting a delivery
//! issues stock through [`super::inventory`]'s engine in the same
//! database transaction, consuming the reservation the order confirmed;
//! posting an invoice books Dr AR / Cr Sales over the GL port.
//!
//! Pricing is a resolution chain, not a rule engine: manual override
//! (permission-gated, floored at the item's minimum selling price) →
//! customer price list → customer-group list → default lists →
//! `item.selling_price` — and every line records where its price came
//! from, so "why this price?" is always answerable.

pub mod credit_note;
pub mod customer;
pub mod delivery;
pub mod documents;
pub mod invoice;
pub mod order;
pub mod payment;
pub mod pricing;
pub mod quotation;
pub mod reports;
pub mod widgets;

pub mod permissions {
    use nebula::auth::PermissionDef;

    pub mod names {
        pub const SALES: &str = "Pages.Sales";
        pub const CUSTOMERS: &str = "Pages.Sales.Customers";
        pub const CUSTOMERS_VIEW: &str = "Pages.Sales.Customers.View";
        pub const CUSTOMERS_CREATE: &str = "Pages.Sales.Customers.Create";
        pub const CUSTOMERS_EDIT: &str = "Pages.Sales.Customers.Edit";
        pub const CUSTOMERS_DELETE: &str = "Pages.Sales.Customers.Delete";
        pub const PRICING: &str = "Pages.Sales.Pricing";
        pub const PRICING_VIEW: &str = "Pages.Sales.Pricing.View";
        pub const PRICING_MANAGE: &str = "Pages.Sales.Pricing.Manage";
        pub const PRICING_OVERRIDE: &str = "Pages.Sales.Pricing.Override";
        pub const QUOTATIONS: &str = "Pages.Sales.Quotations";
        pub const QUOTATIONS_VIEW: &str = "Pages.Sales.Quotations.View";
        pub const QUOTATIONS_CREATE: &str = "Pages.Sales.Quotations.Create";
        pub const QUOTATIONS_SEND: &str = "Pages.Sales.Quotations.Send";
        pub const QUOTATIONS_CONVERT: &str = "Pages.Sales.Quotations.Convert";
        pub const ORDERS: &str = "Pages.Sales.Orders";
        pub const ORDERS_VIEW: &str = "Pages.Sales.Orders.View";
        pub const ORDERS_CREATE: &str = "Pages.Sales.Orders.Create";
        pub const ORDERS_CONFIRM: &str = "Pages.Sales.Orders.Confirm";
        pub const ORDERS_CANCEL: &str = "Pages.Sales.Orders.Cancel";
        pub const ORDERS_CLOSE: &str = "Pages.Sales.Orders.Close";
        pub const CREDIT_OVERRIDE: &str = "Pages.Sales.Credit.Override";
        pub const DELIVERIES: &str = "Pages.Sales.Deliveries";
        pub const DELIVERIES_VIEW: &str = "Pages.Sales.Deliveries.View";
        pub const DELIVERIES_CREATE: &str = "Pages.Sales.Deliveries.Create";
        pub const DELIVERIES_POST: &str = "Pages.Sales.Deliveries.Post";
        pub const DELIVERIES_REVERSE: &str = "Pages.Sales.Deliveries.Reverse";
        pub const INVOICES: &str = "Pages.Sales.Invoices";
        pub const INVOICES_VIEW: &str = "Pages.Sales.Invoices.View";
        pub const INVOICES_CREATE: &str = "Pages.Sales.Invoices.Create";
        pub const INVOICES_POST: &str = "Pages.Sales.Invoices.Post";
        pub const INVOICES_CANCEL: &str = "Pages.Sales.Invoices.Cancel";
        pub const CREDIT_NOTES: &str = "Pages.Sales.CreditNotes";
        pub const CREDIT_NOTES_VIEW: &str = "Pages.Sales.CreditNotes.View";
        pub const CREDIT_NOTES_CREATE: &str = "Pages.Sales.CreditNotes.Create";
        pub const CREDIT_NOTES_POST: &str = "Pages.Sales.CreditNotes.Post";
        pub const CREDIT_NOTES_CANCEL: &str = "Pages.Sales.CreditNotes.Cancel";
        pub const PAYMENTS: &str = "Pages.Sales.Payments";
        pub const PAYMENTS_VIEW: &str = "Pages.Sales.Payments.View";
        pub const PAYMENTS_CREATE: &str = "Pages.Sales.Payments.Create";
        pub const PAYMENTS_POST: &str = "Pages.Sales.Payments.Post";
        pub const PAYMENTS_REVERSE: &str = "Pages.Sales.Payments.Reverse";
        pub const REPORTS: &str = "Pages.Sales.Reports";
        pub const REPORTS_VIEW: &str = "Pages.Sales.Reports.View";
    }

    pub fn tree() -> PermissionDef {
        use names::*;
        PermissionDef::new(SALES, "Sales")
            .child(
                PermissionDef::new(CUSTOMERS, "Customers")
                    .child(PermissionDef::new(CUSTOMERS_VIEW, "View customers"))
                    .child(PermissionDef::new(CUSTOMERS_CREATE, "Create customers"))
                    .child(PermissionDef::new(CUSTOMERS_EDIT, "Edit customers"))
                    .child(PermissionDef::new(CUSTOMERS_DELETE, "Delete customers")),
            )
            .child(
                PermissionDef::new(PRICING, "Pricing")
                    .child(PermissionDef::new(PRICING_VIEW, "View price lists"))
                    .child(PermissionDef::new(PRICING_MANAGE, "Manage price lists"))
                    .child(PermissionDef::new(PRICING_OVERRIDE, "Override line prices")),
            )
            .child(
                PermissionDef::new(QUOTATIONS, "Quotations")
                    .child(PermissionDef::new(QUOTATIONS_VIEW, "View quotations"))
                    .child(PermissionDef::new(QUOTATIONS_CREATE, "Create quotations"))
                    .child(PermissionDef::new(QUOTATIONS_SEND, "Send quotations"))
                    .child(PermissionDef::new(
                        QUOTATIONS_CONVERT,
                        "Convert quotations to orders",
                    )),
            )
            .child(
                PermissionDef::new(ORDERS, "Sales orders")
                    .child(PermissionDef::new(ORDERS_VIEW, "View sales orders"))
                    .child(PermissionDef::new(ORDERS_CREATE, "Create sales orders"))
                    .child(PermissionDef::new(ORDERS_CONFIRM, "Confirm sales orders"))
                    .child(PermissionDef::new(ORDERS_CANCEL, "Cancel sales orders"))
                    .child(PermissionDef::new(ORDERS_CLOSE, "Close sales orders")),
            )
            .child(PermissionDef::new(
                CREDIT_OVERRIDE,
                "Override the credit limit check",
            ))
            .child(
                PermissionDef::new(DELIVERIES, "Deliveries")
                    .child(PermissionDef::new(DELIVERIES_VIEW, "View deliveries"))
                    .child(PermissionDef::new(DELIVERIES_CREATE, "Create deliveries"))
                    .child(PermissionDef::new(DELIVERIES_POST, "Post deliveries"))
                    .child(PermissionDef::new(DELIVERIES_REVERSE, "Reverse deliveries")),
            )
            .child(
                PermissionDef::new(INVOICES, "Sales invoices")
                    .child(PermissionDef::new(INVOICES_VIEW, "View sales invoices"))
                    .child(PermissionDef::new(INVOICES_CREATE, "Create sales invoices"))
                    .child(PermissionDef::new(INVOICES_POST, "Post sales invoices"))
                    .child(PermissionDef::new(INVOICES_CANCEL, "Cancel sales invoices")),
            )
            .child(
                PermissionDef::new(CREDIT_NOTES, "Credit notes")
                    .child(PermissionDef::new(CREDIT_NOTES_VIEW, "View credit notes"))
                    .child(PermissionDef::new(
                        CREDIT_NOTES_CREATE,
                        "Create credit notes",
                    ))
                    .child(PermissionDef::new(CREDIT_NOTES_POST, "Post credit notes"))
                    .child(PermissionDef::new(
                        CREDIT_NOTES_CANCEL,
                        "Cancel credit notes",
                    )),
            )
            .child(
                PermissionDef::new(PAYMENTS, "Customer payments")
                    .child(PermissionDef::new(PAYMENTS_VIEW, "View customer payments"))
                    .child(PermissionDef::new(
                        PAYMENTS_CREATE,
                        "Create customer payments",
                    ))
                    .child(PermissionDef::new(PAYMENTS_POST, "Post customer payments"))
                    .child(PermissionDef::new(
                        PAYMENTS_REVERSE,
                        "Reverse customer payments",
                    )),
            )
            .child(
                PermissionDef::new(REPORTS, "Sales reports")
                    .child(PermissionDef::new(REPORTS_VIEW, "View sales reports")),
            )
    }
}
