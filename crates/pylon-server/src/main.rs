use pylon::Kernel;

#[tokio::main]
async fn main() -> pylon::Result<()> {
    Kernel::builder().build()?.run().await
}
