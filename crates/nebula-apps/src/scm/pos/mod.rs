//! Point of sale: fast tills selling against the SCM machinery.
//!
//! The fourth submodule of [`crate::scm`], and deliberately the lightest:
//! a sale at the counter writes one `pos_orders` document — no stock
//! ledger rows, no GL, no sales-order machinery per ticket. The session
//! is the unit of accountability (register → open with a float → sales →
//! paid-in/out → closing count → over/short), and closing a session
//! consolidates the whole day in one transaction: one aggregated stock
//! issue through the inventory engine and one revenue GL request staged
//! in the scm outbox. Fast tills, consolidated books.
//!
//! - [`register`] — tills: where sales happen, tied to the warehouse
//!   they sell from, carrying the client's tile grid layout
//! - [`session`] — one cashier's span at one register: open, drawer
//!   paid-in/out, X report, close + consolidation, Z report
//! - [`sale`] — POS orders: capture (online and offline-synced,
//!   idempotent on the client UUID), voids, refunds, the catalog feed
//! - [`settings`] — tenant-wide POS behaviour: blind counts and the
//!   count-sheet denomination set
//! - [`reports`] — the framework-engine reports: session summaries,
//!   tender mix, item and hourly sales, and the printable Z
//!
//! Tenders are cash, M-Pesa and card in v1; their clearing accounts are
//! seeded accounting roles (`mpesa_clearing`, `card_clearing`,
//! `cash_over_short`) — the only accounting-side vocabulary POS adds.

pub mod register;
pub mod reports;
pub mod sale;
pub mod session;
pub mod settings;
pub mod widgets;

pub mod permissions {
    use nebula::auth::PermissionDef;

    pub mod names {
        pub const POS: &str = "Pages.Pos";
        pub const REGISTERS: &str = "Pages.Pos.Registers";
        pub const REGISTERS_VIEW: &str = "Pages.Pos.Registers.View";
        pub const REGISTERS_MANAGE: &str = "Pages.Pos.Registers.Manage";
        pub const SESSIONS: &str = "Pages.Pos.Sessions";
        pub const SESSIONS_OPEN: &str = "Pages.Pos.Sessions.Open";
        pub const SESSIONS_CLOSE: &str = "Pages.Pos.Sessions.Close";
        pub const SESSIONS_PAID_IN_OUT: &str = "Pages.Pos.Sessions.PaidInOut";
        pub const SELL: &str = "Pages.Pos.Sell";
        pub const REFUND: &str = "Pages.Pos.Refund";
        /// The PIN-gated acts: voids, discounts, price overrides.
        pub const OVERRIDE: &str = "Pages.Pos.Override";
        pub const REPORTS: &str = "Pages.Pos.Reports";
        pub const REPORTS_VIEW: &str = "Pages.Pos.Reports.View";
    }

    pub fn tree() -> PermissionDef {
        use names::*;
        PermissionDef::new(POS, "Point of sale")
            .child(
                PermissionDef::new(REGISTERS, "Registers")
                    .child(PermissionDef::new(REGISTERS_VIEW, "View registers"))
                    .child(PermissionDef::new(REGISTERS_MANAGE, "Manage registers")),
            )
            .child(
                PermissionDef::new(SESSIONS, "Sessions")
                    .child(PermissionDef::new(SESSIONS_OPEN, "Open sessions"))
                    .child(PermissionDef::new(SESSIONS_CLOSE, "Close sessions"))
                    .child(PermissionDef::new(
                        SESSIONS_PAID_IN_OUT,
                        "Record drawer paid in / out",
                    )),
            )
            .child(PermissionDef::new(SELL, "Sell at the till"))
            .child(PermissionDef::new(REFUND, "Refund against a receipt"))
            .child(PermissionDef::new(
                OVERRIDE,
                "Override (voids, discounts, prices)",
            ))
            .child(
                PermissionDef::new(REPORTS, "POS reports")
                    .child(PermissionDef::new(REPORTS_VIEW, "View POS reports")),
            )
    }
}
