use nebula::{AuditModule, AuthModule, Kernel};

#[tokio::main]
async fn main() -> nebula::Result<()> {
    Kernel::builder()
        .add_module(AuthModule)
        .add_module(AuditModule)
        .build()?
        .run()
        .await
}
