use nebula::{AuthModule, Kernel};

#[tokio::main]
async fn main() -> nebula::Result<()> {
    Kernel::builder()
        .add_module(AuthModule)
        .build()?
        .run()
        .await
}
