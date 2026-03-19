use clap::Parser;
use lean_mcp_core::config::CliArgs;
use tracing_subscriber::EnvFilter;

mod server;
mod tools;

#[tokio::main]
async fn main() {
    // Initialize tracing (respects RUST_LOG env var).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Parse CLI args.
    let args = CliArgs::parse_from(std::env::args());

    // Create the application context.
    let ctx = server::AppContext::with_options(
        args.lean_project_path.map(std::path::PathBuf::from),
        tools::search::SearchConfig::default(),
    );

    tracing::info!(
        "{} v{} starting on stdio",
        server::server_name(),
        server::server_version()
    );

    // Start MCP server on stdio transport.
    let transport = rmcp::transport::io::stdio();
    let server = match rmcp::serve_server(ctx, transport).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to start MCP server: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = server.waiting().await {
        tracing::error!("MCP server error: {e}");
        std::process::exit(1);
    }
}
