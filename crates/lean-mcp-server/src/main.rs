use clap::Parser;
use lean_mcp_core::config::CliArgs;

mod server;
mod tools;

fn main() {
    // Parse CLI args (validates flags/env vars early).
    let args = CliArgs::parse_from(std::env::args());

    // Create the application context, optionally with a Lean project path.
    let ctx = server::AppContext {
        lean_project_path: args.lean_project_path.map(std::path::PathBuf::from),
    };

    // For now just print server info and exit.
    // Full stdio/HTTP transport will be wired up in a follow-up issue.
    println!("{} v{}", server::server_name(), server::server_version());
    if let Some(ref path) = ctx.lean_project_path {
        println!("Lean project: {}", path.display());
    }
    println!("{}", server::server_instructions());
}
