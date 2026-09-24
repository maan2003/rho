#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    rho_shell::run().await
}
