use nebula::Kernel;

#[tokio::main]
async fn main() -> nebula::Result<()> {
    Kernel::builder().build()?.run().await
}
