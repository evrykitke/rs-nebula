use nebula::Kernel;
use nebula_apps::{AccountingApp, ScmApp, WorkspaceApp};

#[tokio::main]
async fn main() -> nebula::Result<()> {
    Kernel::builder()
        .add_module(WorkspaceApp)
        .add_module(AccountingApp)
        .add_module(ScmApp)
        .build()?
        .run()
        .await
}
