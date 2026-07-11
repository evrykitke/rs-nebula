//! Business apps built on the Nebula framework.
//!
//! An **app** is a user-facing product surface (workspace, and later
//! sales, accounting, inventory). Apps are independent: one app never
//! imports another. When an app needs data another app owns, it consumes
//! it through a framework port (a `ReportDataSource`, an event, or the
//! service registry) — never by depending on the other app's code. Each
//! app implements [`nebula::Module`] and is registered in `main.rs`.
//!
//! For now every app lives in this one crate, one module per app, with a
//! hard rule of no cross-app imports; an app graduates to its own crate
//! when that boundary needs to be compiler-enforced.

pub mod workspace;

pub use workspace::WorkspaceApp;
