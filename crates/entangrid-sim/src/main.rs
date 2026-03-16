#[tokio::main]
async fn main() -> anyhow::Result<()> {
    entangrid_sim::cli_main().await
}
