//! Workspace reports: what the engine can do, with nothing set up yet.
//!
//! - **Workspace Overview** — a tour of every widget the engine offers.
//! - **Sample Register** — a list report, also exported to Excel.
//!
//! Both use only the framework company datasource (resolved automatically), so
//! they need no business data and render on a bare tenant.
//!
//! One report per file, as everywhere else.

pub mod overview;
pub mod sample_register;

pub use overview::WorkspaceOverview;
pub use sample_register::SampleRegister;
