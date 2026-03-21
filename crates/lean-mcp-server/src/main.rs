use clap::Parser;
use lean_mcp_core::config::CliArgs;
use lean_mcp_server::server;
use lean_mcp_server::tools;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Initialize tracing (respects RUST_LOG env var).
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Optional file-based tracing for performance analysis.
    // Set LEAN_MCP_LOG=/tmp/rust-lean-mcp.log to enable.
    let _guard = if let Ok(log_path) = std::env::var("LEAN_MCP_LOG") {
        let parent = std::path::Path::new(&log_path)
            .parent()
            .unwrap_or(std::path::Path::new("/tmp"));
        let filename = std::path::Path::new(&log_path)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("rust-lean-mcp.log"));
        let file_appender = tracing_appender::rolling::never(parent, filename);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_span_events(FmtSpan::CLOSE)
            .with_target(true)
            .with_ansi(false);

        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stderr_layer)
            .init();

        Some(guard)
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
        None
    };

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
