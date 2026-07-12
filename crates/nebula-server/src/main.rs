use nebula::Kernel;
use nebula_apps::{AccountingApp, WorkspaceApp};

#[tokio::main]
async fn main() -> nebula::Result<()> {
    // WorkspaceApp is the default app; it depends on administration, which
    // depends on account, so registering it pulls the whole stack in.
    // AccountingApp adds double-entry bookkeeping (also on administration).
    Kernel::builder()
        .add_module(WorkspaceApp)
        .add_module(AccountingApp)
        .build()?
        .run()
        .await
}
