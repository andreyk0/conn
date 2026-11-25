//! Process entry point; everything lives in the `conn` library.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    conn::cli::run().await
}
