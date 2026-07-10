use nebula::{AuditModule, AuthModule, CurrencyModule, Kernel};

#[tokio::main]
async fn main() -> nebula::Result<()> {
    Kernel::builder()
        .add_module(AuthModule)
        .add_module(AuditModule)
        .add_module(CurrencyModule)
        .build()?
        .run()
        .await
}
