#[tokio::main]
async fn main() -> anyhow::Result<()> {
    entangrid_node::cli_main().await
}
