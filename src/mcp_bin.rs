#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hinta=warn".into()),
        )
        .with_target(false)
        .without_time()
        // stdout carries the JSON-RPC stream, so diagnostics must go to stderr.
        .with_writer(std::io::stderr)
        .init();

    hinta::mcp::run().await;
}
