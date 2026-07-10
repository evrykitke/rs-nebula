//! Audit logging: who did what, from where, and what really changed.
//!
//! - [`log`] — the `audit_logs` entity: full request context (user,
//!   tenant, ip address, user agent, request id) plus before/after
//!   entity snapshots stored as jsonb
//! - [`Recorder`] / [`Audit`] — handlers record `create`/`update`/
//!   `delete` snapshots of the client-safe view of an entity
//! - [`diff`] — field-level comparison of two snapshots, powering the
//!   what-changed view
//! - [`AuditModule`] — ready-made trail endpoints with a per-entry
//!   what-changed diff view, guarded by `Pages.Administration.AuditLogs.View`
//!
//! Request bodies are never recorded — they can carry passwords.
//! Audit writes are failure-contained: they log errors, they do not
//! fail the request they describe.

pub mod diff;
pub mod log;
pub(crate) mod middleware;
pub mod module;
pub mod recorder;

pub use diff::{FieldChange, diff};
pub use module::AuditModule;
pub use recorder::{Audit, Recorder, RequestInfo};
