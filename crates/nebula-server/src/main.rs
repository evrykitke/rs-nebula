use nebula::{AdministrationModule, Kernel};

#[tokio::main]
async fn main() -> nebula::Result<()> {
    // AccountModule comes in through AdministrationModule's depends_on.
    Kernel::builder()
        .add_module(AdministrationModule)
        .build()?
        .run()
        .await
}
