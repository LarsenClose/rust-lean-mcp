# rust-lean-mcp

A high-performance Rust implementation of [lean-lsp-mcp](https://github.com/oOo0oOo/lean-lsp-mcp) — a Model Context Protocol (MCP) server that bridges Lean 4's Language Server Protocol to AI assistants.

## Installation

### From source

```bash
git clone https://github.com/LarsenClose/rust-lean-mcp.git
cd rust-lean-mcp
cargo install --path crates/lean-mcp-server
```

### Prerequisites

- [Lean 4](https://leanprover.github.io/lean4/doc/setup.html) with `lake` on your PATH
- A Lean project with `lakefile.lean` (or `lakefile.toml`) and `lean-toolchain`

## Usage

### With Claude Code

Add to your `~/.claude.json` (global) or project `.mcp.json`:

```json
{
  "mcpServers": {
    "lean-lsp": {
      "command": "rust-lean-mcp"
    }
  }
}
```

The server auto-detects your Lean project from the file paths in tool calls — no `--lean-project-path` needed for most workflows.

### With explicit project path

```bash
rust-lean-mcp --lean-project-path /path/to/lean/project
```

### Auto-detection

When `--lean-project-path` is omitted, the server detects the Lean project root automatically:

1. From the `file_path` argument of each tool call (walks up looking for `lakefile.lean`, `lakefile.toml`, or `lean-toolchain`)
2. From the server's working directory
3. Falls back to an error with a clear message

Multiple Lean projects work in the same session — each gets its own LSP client.

## Tools

26 MCP tools for Lean 4 proof assistance:

| Category | Tools |
|----------|-------|
| **Proof state** | `lean_goal`, `lean_term_goal`, `lean_proof_diff`, `lean_goals_batch` |
| **Code intelligence** | `lean_hover_info`, `lean_completions`, `lean_references`, `lean_declaration_file`, `lean_code_actions` |
| **File analysis** | `lean_diagnostic_messages`, `lean_file_outline`, `lean_project_health` |
| **Search** | `lean_leansearch`, `lean_loogle`, `lean_leanfinder`, `lean_state_search`, `lean_hammer_premise`, `lean_local_search` |
| **Tactics** | `lean_multi_attempt` (parallel and sequential), `lean_run_code` |
| **Build** | `lean_build` |
| **Verification** | `lean_verify` |
| **Widgets** | `lean_get_widgets`, `lean_get_widget_source` |
| **Profiling** | `lean_profile_proof` |
| **Batch** | `lean_batch` |

## Architecture

3-crate workspace:

```
crates/
├── lean-lsp-client/   # Standalone async Lean 4 LSP client (no MCP dependency)
├── lean-mcp-core/     # Business logic, models, utilities (no MCP/LSP dependency)
└── lean-mcp-server/   # MCP server binary (rmcp + tool handlers)
```

## Development

```bash
cargo test --all
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

CI runs 6 required checks on every PR: Rustfmt, Clippy, Tests (ubuntu + macOS), Documentation, and Coverage.

## License

MIT
