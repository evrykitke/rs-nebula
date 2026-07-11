use nebula::Kernel;
use nebula_apps::WorkspaceApp;

#[tokio::main]
async fn main() -> nebula::Result<()> {
    // WorkspaceApp is the default app; it depends on administration, which
    // depends on account, so registering it pulls the whole stack in.
    Kernel::builder()
        .add_module(WorkspaceApp)
        .build()?
        .run()
        .await
}
